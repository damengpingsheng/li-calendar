// lical_clock_tap — C 阶段 TAP DLL（方案 v2 §5 C；v30=B 阶段，v31 起=C0 探针/C1 面板）
// 纪律（方案 §6）：DllMain 仅返回 TRUE；不调用 DisableThreadLibraryCalls（/MT 静态 CRT）；
// 全 COM 入口 try/catch(...) 包裹；任何异常/失败一律躺平记日志，不重试。
// C 阶段沿用 B 阶段全部七条血泪（见下）+ 新增：树回调内零引擎调用零锁等待纪律
// 同样适用于面板维护定时器与输入事件回调；面板/探针元素一律 winrt strong ref，
// 仅在 UI 线程触碰；一切改动带快照与代次令牌。
//
// 【B 阶段血泪记录（实现已按此设计，勿回退）】
// #1 失败路径自钉：初始化 60 次失败后 host 卸钩 → 钩子引用归零 DLL 被卸 → pipe/flush 线程
//    执行已卸载代码 → explorer 0xC0000005（2026-09-12 21:49 实测）。DllMain 内
//    GET_MODULE_HANDLE_EX_FLAG_PIN 自钉（成功时引擎本就永久钉住；模块暂留=D8 分期语义）。
// #2 树事件到达顺序不保证父先于子 → 定位扫描必须在每个 ADD 事件上尝试，不能只认 Time 命名事件。
// #3 新鲜 explorer 的 XAML 诊断端口沉降期：~90s 内 init 报 0x80070490（E_NOTFOUND），
//    ~2.5min 时 advise 成功但零重放，~4.5min 起全链路正常（重试循环必须容忍，勿缩短放弃）。
// #4 定位链的兄弟校验必须比较 Time 的父（StackPanel 句柄）；写成 StackPanel 的父是比到
//    祖父（ContainerGrid），永假（离线回放仿真逐级打印定位，sim.cpp）。
// #5 AdviseVisualTreeChange 是同步语义：阻塞到整树重放在 UI 线程完成才返回。回调内
//    绝不能等任何被 advise 线程持有的锁（v20~v22 实测 UI 线程死锁挂死任务栏）；
//    回调内也绝不做引擎调用（GetIInspectableFromHandle 等一律移到 dispatched 上下文）。
//    接口指针 = 全局会话级（SetSite 设置，先于 Advise，线程创建即 happens-before；
//    引擎与 DLL 同生命周期，不释放——模块钉死语义下等价安全）。
//
// build: build_tap.cmd（cl /LD /MT /EHsc /std:c++17 /utf-8）
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <objbase.h>
#include <ocidl.h>
#include <xamlom.h>
#include <stdio.h>
#include <string>
#include <unordered_map>

#pragma push_macro("GetCurrentTime")
#undef GetCurrentTime
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Foundation.Collections.h>
#include <winrt/Windows.UI.h>
#include <winrt/Windows.UI.Core.h>
#include <winrt/Windows.UI.Text.h>
#include <winrt/Windows.UI.Xaml.h>
#include <winrt/Windows.UI.Xaml.Controls.h>
#include <winrt/Windows.UI.Xaml.Controls.Primitives.h>
#include <winrt/Windows.UI.Xaml.Input.h>
#include <winrt/Windows.UI.Xaml.Media.h>
#pragma pop_macro("GetCurrentTime")
namespace wux = winrt::Windows::UI::Xaml;
namespace wuc = winrt::Windows::UI::Core;
namespace wuxm = winrt::Windows::UI::Xaml::Media;
namespace wfnd = winrt::Windows::Foundation;

// ── 身份 ──────────────────────────────────────────────
// CLSID_LicalClockTap { D4C1B77E-4E2F-4E7A-9B31-5F0A6C2E8B14 }
static const CLSID CLSID_LicalClockTap = {
    0xd4c1b77e, 0x4e2f, 0x4e7a, {0x9b, 0x31, 0x5f, 0x0a, 0x6c, 0x2e, 0x8b, 0x14} };

static const IID IID_IVisualTreeService_ = {
    0xa593b11a, 0xd17f, 0x48bb, {0x8f, 0x66, 0x83, 0x91, 0x07, 0x31, 0xc8, 0xa5} };
// IVisualTreeService3 {0E79C6E0-85A0-4BE8-B41A-655CF1FD19BD}（含 v1+v2 全部槽位）
static const IID IID_IVisualTreeService3_ = {
    0x0e79c6e0, 0x85a0, 0x4be8, {0xb4, 0x1a, 0x65, 0x5c, 0xf1, 0xfd, 0x19, 0xbd} };
static const IID IID_IVisualTreeServiceCallback2_ = {
    0xbad9eb88, 0xae77, 0x4397, {0xb9, 0x48, 0x5f, 0xa2, 0xdb, 0x0a, 0x19, 0xea} };
static const IID IID_IXamlDiagnostics_ = {
    0x18c9e2b6, 0x3f43, 0x4116, {0x9f, 0x2b, 0xff, 0x93, 0x5d, 0x77, 0x70, 0xd2} };

#ifndef TAPVER
#define TAPVER 0
#endif
static wchar_t g_pipeName[96];
static wchar_t g_logPath[96];

static void init_paths() {
    _snwprintf_s(g_pipeName, 96, _TRUNCATE, L"\\\\.\\pipe\\lical-clockbar-b%d", TAPVER);
    _snwprintf_s(g_logPath, 96, _TRUNCATE, L"D:\\agents_tmp\\clockbar_tap_b%d.log", TAPVER);
}

// ── 日志：内存缓冲 + 后台落盘（回调线程只做拼接）─────
static SRWLOCK        g_logLock = SRWLOCK_INIT;
static std::string    g_logBuf;
static volatile LONG  g_logLines = 0;
static const LONG     kMaxLoggedLines = 1500000;
static volatile LONG  g_flushQueued = 0;
static volatile LONG  g_objectsAlive = 0;
static volatile LONG  g_adviseCount = 0;
static volatile LONG  g_unadviseCount = 0;

static void log_line(const char* fmt, ...) {
    InterlockedIncrement(&g_logLines);
    if (g_logLines > kMaxLoggedLines) return;
    char tmp[1400];
    SYSTEMTIME st; GetLocalTime(&st);
    int head = _snprintf_s(tmp, sizeof(tmp), _TRUNCATE,
        "[%02d:%02d:%02d.%03d][tid=%lu] ", st.wHour, st.wMinute, st.wSecond, st.wMilliseconds,
        GetCurrentThreadId());
    va_list ap; va_start(ap, fmt);
    _vsnprintf_s(tmp + head, sizeof(tmp) - head - 2, _TRUNCATE, fmt, ap);
    va_end(ap);
    strcat_s(tmp, "\n");
    AcquireSRWLockExclusive(&g_logLock);
    g_logBuf += tmp;
    if (g_logBuf.size() > (4u << 20)) g_logBuf.erase(0, g_logBuf.size() - (1u << 20));
    ReleaseSRWLockExclusive(&g_logLock);
    InterlockedExchange(&g_flushQueued, 1);
}

static DWORD WINAPI flush_thread(LPVOID) {
    // v34：常开句柄（v33 实测每次开关句柄会与 AV/索引器竞争导致落盘线程长时间卡死，
    // 日志窗口丢失）。写失败才重开。
    HANDLE h = INVALID_HANDLE_VALUE;
    for (;;) {
        Sleep(300);
        if (!InterlockedExchange(&g_flushQueued, 0)) continue;
        std::string out;
        AcquireSRWLockExclusive(&g_logLock);
        out.swap(g_logBuf);
        ReleaseSRWLockExclusive(&g_logLock);
        if (out.empty()) continue;
        if (h == INVALID_HANDLE_VALUE)
            h = CreateFileW(g_logPath, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE,
                            NULL, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
        if (h == INVALID_HANDLE_VALUE) continue;
        DWORD written = 0;
        if (!WriteFile(h, out.data(), (DWORD)out.size(), &written, NULL)) {
            CloseHandle(h);
            h = INVALID_HANDLE_VALUE;
            continue;
        }
    }
}

static void ensure_flush_thread() {
    static LONG started = 0;
    if (!InterlockedExchange(&started, 1))
        CreateThread(NULL, 0, flush_thread, NULL, 0, NULL);
}

// ── 会话级状态（全局；单 TapObject 语义：SetSite 设置一次，接口不释放，见血泪 #5）──
static IXamlDiagnostics*    g_diag = nullptr;   // SetSite 设置，先于 Advise（线程创建=同步点）
static IVisualTreeService3* g_svc = nullptr;    // 同上
static wuc::CoreDispatcher  g_disp{ nullptr };  // advise 线程在 Advise 返回后 GetDispatcher（agile）

// 管道句柄（pipe 线程写，供 tap→host 主动消息）
static volatile HANDLE g_pipeH = INVALID_HANDLE_VALUE;

// ── 树跟踪 + 快照 + 代次（g_stateLock 保护）───────────
struct Node {
    std::string type, name;
    unsigned long long parent;
};
static SRWLOCK g_stateLock = SRWLOCK_INIT;
static std::unordered_map<unsigned long long, Node> g_nodes;
static bool     g_located = false;
static unsigned g_gen = 0;
static unsigned long long g_hTime = 0, g_hDate = 0, g_hStack = 0, g_hCont = 0, g_hDTIC = 0;

// 【B 阶段血泪 #6】tap→host 发送串行化：ready 由 explorer UI 线程（树回调内）发，
// loaded/ack 由 pipe 线程发——同一同步句柄双线程并发 WriteFile 会导致字节流损坏/
// 消息丢失（实测 host 每24s会话必丢 ack；A 阶段全部发送单线程故未暴露）。
// 所有发送必须持 g_sendLock。注意：UI 线程持锁期间 WriteFile 若阻塞会拖住任务栏——
// 管道写缓冲 64KB、报文 <1KB，仅在对端死连接时可能阻塞，可接受（对端死=会话已失效）。
static SRWLOCK g_sendLock = SRWLOCK_INIT;
static void send_line(const char* s) {
    HANDLE h = g_pipeH; if (h == INVALID_HANDLE_VALUE) { log_line("SEND skip(no pipe): %s", s); return; }
    DWORD w = 0;
    AcquireSRWLockExclusive(&g_sendLock);
    h = g_pipeH; // 双检：等待锁期间可能断线换句柄
    BOOL ok = FALSE;
    if (h != INVALID_HANDLE_VALUE)
        ok = WriteFile(h, s, (DWORD)strlen(s), &w, NULL);
    ReleaseSRWLockExclusive(&g_sendLock);
    log_line("SEND h=%llX ok=%d w=%lu: %s", (unsigned long long)(uintptr_t)h, (int)ok, (unsigned long)w, s);
}
static void w2a(PCWSTR w, char* a, size_t n) {
    a[0] = 0;
    if (w) WideCharToMultiByte(CP_UTF8, 0, w, -1, a, (int)n, NULL, NULL);
}

struct Snap {
    bool     modified = false;
    int      route = 0;            // 1=A(SetProperty) 2=B(winRT 直设)
    unsigned gen = 0;
    unsigned propIdx = 0; bool hasPropIdx = false;
    wchar_t  origText[128] = L"";  bool hasOrigText = false;
    wchar_t  written[128] = L"";
};
static Snap g_snap;

// C1 面板前置声明（定义在文件后部；树回调只置标志/清引用，零引擎调用零锁等待）
static void   PanelOrphan();
static HRESULT PanelBuild(bool rebuildAfterGen);
static bool   SelfReparentPositive();
static volatile LONG g_panelWanted = 0;  // host 要求面板在场（c1set 置位，c1free/断线清零）
static volatile LONG g_panelOn = 0;      // 面板在场标志（定义于 C1 段，此处仅供树回调判读）
static bool g_hasData = false;           // host 下发的段文案缓存（重建用）
static volatile LONG g_selfReparent = 0; // reparent 期间的自我 REM 抑制（v35，定义于 C1 段）

// locate 判定（调用方持 g_stateLock）：链 = DTIC → ContainerGrid → 无名 StackPanel → Time/Date
static bool chain_matches(const std::unordered_map<unsigned long long, Node>& m,
                          unsigned long long hTime, unsigned long long hDate) {
    auto itT = m.find(hTime); if (itT == m.end()) return false;
    if (itT->second.name != "TimeInnerTextBlock") return false;
    auto itS = m.find(itT->second.parent); if (itS == m.end()) return false;
    if (itS->second.type.find("StackPanel") == std::string::npos) return false;
    if (!itS->second.name.empty()) return false;
    auto itC = m.find(itS->second.parent); if (itC == m.end()) return false;
    if (itC->second.name != "ContainerGrid") return false;
    auto itD = m.find(itC->second.parent); if (itD == m.end()) return false;
    if (itD->second.type != "SystemTray.DateTimeIconContent") return false;
    auto itDt = m.find(hDate);
    // 血泪 #4：兄弟校验比 Time 的父（StackPanel），不是 StackPanel 的父
    return itDt != m.end() && itDt->second.name == "DateInnerTextBlock"
        && itDt->second.parent == itT->second.parent;
}

static void free_chain_arrays(unsigned srcCount, PropertyChainSource* srcs,
                              unsigned propCount, PropertyChainValue* vals) {
    for (unsigned i = 0; i < srcCount && srcs; i++) {
        if (srcs[i].TargetType) SysFreeString(srcs[i].TargetType);
        if (srcs[i].Name)       SysFreeString(srcs[i].Name);
        if (srcs[i].SrcInfo.FileName) SysFreeString(srcs[i].SrcInfo.FileName);
        if (srcs[i].SrcInfo.Hash)     SysFreeString(srcs[i].SrcInfo.Hash);
    }
    for (unsigned i = 0; i < propCount && vals; i++) {
        if (vals[i].Type)          SysFreeString(vals[i].Type);
        if (vals[i].DeclaringType) SysFreeString(vals[i].DeclaringType);
        if (vals[i].ValueType)     SysFreeString(vals[i].ValueType);
        if (vals[i].ItemType)      SysFreeString(vals[i].ItemType);
        if (vals[i].Value)         SysFreeString(vals[i].Value);
        if (vals[i].PropertyName)  SysFreeString(vals[i].PropertyName);
    }
    if (srcs) CoTaskMemFree(srcs);
    if (vals) CoTaskMemFree(vals);
}

// ── TAP 对象 ──────────────────────────────────────────
class TapObject;
static SRWLOCK   g_objLock = SRWLOCK_INIT;
static TapObject* g_cur = nullptr;
class TapObject : public IObjectWithSite, public IVisualTreeServiceCallback2 {
    LONG                 m_ref = 1;
    bool                 m_advised = false;
    SRWLOCK              m_lock = SRWLOCK_INIT;   // 仅护 m_advised（Advise/Unadvise 往返）

    HRESULT AdviseLocked() {
        if (m_advised || !g_svc) return S_OK;
        LONG before = InterlockedIncrement(&g_adviseCount);
        // 同步语义（血泪 #5）：本调用阻塞到整树重放在 UI 线程完成；期间 m_lock 必须持有——
        // 因此回调路径绝不可碰 m_lock。
        HRESULT hr = g_svc->AdviseVisualTreeChange(
            static_cast<IVisualTreeServiceCallback*>(this));
        log_line("AdviseVisualTreeChange hr=0x%08lx (advises=%ld, tid=%lu)",
                 (unsigned long)hr, before, GetCurrentThreadId());
        m_advised = SUCCEEDED(hr);
        if (!m_advised) InterlockedDecrement(&g_adviseCount);
        return hr;
    }
    void AdviseFromThread() {
        AddRef();
        HANDLE t = CreateThread(NULL, 0, [](LPVOID p) -> DWORD {
            TapObject* o = (TapObject*)p;
            try {
                AcquireSRWLockExclusive(&o->m_lock);
                o->AdviseLocked();
                ReleaseSRWLockExclusive(&o->m_lock);
                // 重放已完成：此时可安全向引擎要 dispatcher（非回调上下文）
                if (!g_disp && g_diag) {
                    log_line("DISP calling GetDispatcher...");
                    wfnd::IInspectable d;
                    HRESULT hr2 = g_diag->GetDispatcher(
                        reinterpret_cast<::IInspectable**>(winrt::put_abi(d)));
                    log_line("DISP GetDispatcher hr=0x%08lx", (unsigned long)hr2);
                    if (SUCCEEDED(hr2)) {
                        auto cd = d.try_as<wuc::CoreDispatcher>();
                        if (cd) g_disp = cd;
                        log_line("DISP dispatcher=%d", cd ? 1 : 0);
                    }
                }
            } catch (const winrt::hresult_error& e) {
                log_line("advise thread winrt exception hr=0x%08lx", (unsigned long)e.code().value);
            } catch (...) { log_line("advise thread EXCEPTION"); }
            o->Release();
            return 0;
        }, this, 0, NULL);
        if (t) CloseHandle(t); else Release();
    }
    void UnadviseLocked() {
        if (!m_advised) return;
        m_advised = false;
        if (g_svc) {
            InterlockedIncrement(&g_unadviseCount);
            HRESULT hr = g_svc->UnadviseVisualTreeChange(
                static_cast<IVisualTreeServiceCallback*>(this));
            log_line("UnadviseVisualTreeChange hr=0x%08lx", (unsigned long)hr);
        }
    }

    // 树回调（UI 线程；重放由 advise 线程的 AdviseVisualTreeChange 同步等待）。
    // 铁律（血泪 #5）：只更新节点图/定位/发消息；零引擎调用、零 m_lock、零 g_stateLock 等待。
    void TrackTree(ParentChildRelation relation, VisualElement element,
                   VisualMutationType mutationType) {
        char typeA[96] = "", nameA[96] = "";
        if (element.Type) WideCharToMultiByte(CP_UTF8, 0, element.Type, -1, typeA, sizeof(typeA), NULL, NULL);
        if (element.Name) WideCharToMultiByte(CP_UTF8, 0, element.Name, -1, nameA, sizeof(nameA), NULL, NULL);
        bool isTracked = false;
        unsigned long long h = (unsigned long long)element.Handle;
        bool newlyLocated = false;
        AcquireSRWLockExclusive(&g_stateLock);
        if (mutationType == VisualMutationType::Add) {
            if (g_nodes.size() > 8192) g_nodes.clear();
            Node& n = g_nodes[h];
            n.type = typeA; n.name = nameA; n.parent = (unsigned long long)relation.Parent;
            isTracked = (h == g_hTime || h == g_hDate || h == g_hStack || h == g_hCont || h == g_hDTIC);
            if (!g_located) {
                for (auto& [hT, nT] : g_nodes) {
                    if (nT.name != "TimeInnerTextBlock") continue;
                    for (auto& [hD, nD] : g_nodes) {
                        if (nD.name == "DateInnerTextBlock" && chain_matches(g_nodes, hT, hD)) {
                            g_hTime = hT; g_hDate = hD; g_hStack = g_nodes[hT].parent;
                            g_hCont = g_nodes[g_hStack].parent; g_hDTIC = g_nodes[g_hCont].parent;
                            g_located = true; g_gen++; newlyLocated = true;
                            break;
                        }
                    }
                    if (g_located) break;
                }
            }
        } else {
            isTracked = (h == g_hTime || h == g_hDate || h == g_hStack || h == g_hCont || h == g_hDTIC);
            // v35：reparent 自我抑制——构建 lambda 把 Time 移出原生面板时引擎同步报 REM，
            // 这不是树重建。双重判据：构建窗口标志 + 正向校验（Time 现父=自建横板），
            // 后者防引擎队列化回调导致的时序漂移。
            if (isTracked && h == g_hTime && (g_selfReparent || SelfReparentPositive())) {
                log_line("TRACK suppress self-reparent REM h=%llX", h);
                ReleaseSRWLockExclusive(&g_stateLock);
                return;
            }
            g_nodes.erase(h);
            if (isTracked && g_located) {
                unsigned prevGen = g_gen;
                bool wasModified = g_snap.modified;
                log_line("TRACK lost: tracked element REM h=%llX (gen %u -> %u)",
                         h, prevGen, prevGen + 1);
                g_located = false; g_gen++;
                if (g_snap.modified && g_snap.gen == prevGen) {
                    // 元素连同我们的修改一起被系统销毁——快照作废，绝不向新元素抢写旧值
                    log_line("TRACK snapshot orphaned at gen %u (route %d)",
                             prevGen, g_snap.route);
                    g_snap = Snap{};
                }
                if (g_panelOn) {
                    // 面板元素属于旧树：引用全部作废（gen 令牌失配，重建在 relocate 后）
                    log_line("TRACK panel orphaned at gen %u — refs dropped, rebuild on relocate", prevGen);
                    PanelOrphan();
                }
                ReleaseSRWLockExclusive(&g_stateLock);
                char out[192];
                _snprintf_s(out, sizeof(out), _TRUNCATE,
                    "{\"v\":2,\"t\":\"lost\",\"gen\":%u,\"wasModified\":%d}\n",
                    prevGen + 1, (int)wasModified);
                send_line(out);
                AcquireSRWLockExclusive(&g_stateLock);
            }
        }
        ReleaseSRWLockExclusive(&g_stateLock);
        if (newlyLocated) {
            log_line("TRACK located gen=%u DTIC=%llX Cont=%llX Stack=%llX Time=%llX Date=%llX",
                     g_gen, g_hDTIC, g_hCont, g_hStack, g_hTime, g_hDate);
            char out[256];
            _snprintf_s(out, sizeof(out), _TRUNCATE,
                "{\"v\":2,\"t\":\"ready\",\"gen\":%u,\"hTime\":\"%llX\"}\n", g_gen, g_hTime);
            send_line(out);
            // C1：gen 重建后自动重建面板（host 数据缓存仍在；RunAsync 只入队，非引擎调用）
            if (g_panelWanted && g_hasData) {
                wuc::CoreDispatcher disp{ nullptr };
                AcquireSRWLockShared(&g_stateLock); disp = g_disp; ReleaseSRWLockShared(&g_stateLock);
                if (disp) {
                    disp.RunAsync(wuc::CoreDispatcherPriority::Normal, []() {
                        try {
                            HRESULT hr2 = PanelBuild(true);
                            log_line("PANEL auto-rebuild hr=0x%08lx", (unsigned long)hr2);
                        } catch (...) { log_line("PANEL auto-rebuild EXCEPTION"); }
                    });
                }
            }
        }
    }

public:
    TapObject() { InterlockedIncrement(&g_objectsAlive); log_line("TapObject ctor (alive=%ld)", g_objectsAlive); }
    ~TapObject() {
        AcquireSRWLockExclusive(&m_lock);
        UnadviseLocked();
        ReleaseSRWLockExclusive(&m_lock);
        InterlockedDecrement(&g_objectsAlive);
        log_line("TapObject dtor (alive=%ld)", g_objectsAlive);
    }

    void UnadvisePublic() {
        AcquireSRWLockExclusive(&m_lock);
        UnadviseLocked();
        ReleaseSRWLockExclusive(&m_lock);
    }
    void AdvisePublic() { AdviseFromThread(); }

    // ── IUnknown ──
    STDMETHODIMP QueryInterface(REFIID riid, void** ppv) {
        if (!ppv) return E_POINTER;
        if (IsEqualIID(riid, IID_IUnknown) || IsEqualIID(riid, IID_IObjectWithSite)) {
            *ppv = static_cast<IObjectWithSite*>(this);
        } else if (IsEqualIID(riid, __uuidof(IVisualTreeServiceCallback)) ||
                   IsEqualIID(riid, IID_IVisualTreeServiceCallback2_)) {
            *ppv = static_cast<IVisualTreeServiceCallback2*>(this);
        } else {
            *ppv = nullptr; return E_NOINTERFACE;
        }
        reinterpret_cast<IUnknown*>(*ppv)->AddRef();
        return S_OK;
    }
    STDMETHODIMP_(ULONG) AddRef()  { return InterlockedIncrement(&m_ref); }
    STDMETHODIMP_(ULONG) Release() {
        LONG r = InterlockedDecrement(&m_ref);
        if (r == 0) delete this;
        return r;
    }

    // IObjectWithSite
    STDMETHODIMP SetSite(IUnknown* pUnkSite) {
        try {
            ensure_flush_thread();
            if (!pUnkSite) { log_line("SetSite(NULL) — engine going away"); return S_OK; }
            {
                AcquireSRWLockShared(&g_objLock); TapObject* prev = g_cur; ReleaseSRWLockShared(&g_objLock);
                if (prev && prev != this) prev->UnadvisePublic();
            }
            AcquireSRWLockExclusive(&m_lock);
            UnadviseLocked();
            HRESULT hr1 = pUnkSite->QueryInterface(IID_IXamlDiagnostics_, (void**)&g_diag);
            HRESULT hr2 = pUnkSite->QueryInterface(IID_IVisualTreeService3_, (void**)&g_svc);
            int svcVer = 3;
            if (FAILED(hr2)) {
                hr2 = pUnkSite->QueryInterface(IID_IVisualTreeService_, (void**)&g_svc);
                svcVer = 1;
            }
            log_line("SetSite: QI IXamlDiagnostics=0x%08lx IVisualTreeService(v%d)=0x%08lx",
                     (unsigned long)hr1, svcVer, (unsigned long)hr2);
            if (g_svc) AdviseFromThread();   // 接口全局副本已就位（happens-before：线程创建）
            ReleaseSRWLockExclusive(&m_lock);
            return S_OK;
        } catch (...) { log_line("SetSite EXCEPTION"); return E_FAIL; }
    }
    STDMETHODIMP GetSite(REFIID riid, void** ppvSite) {
        if (!ppvSite) return E_POINTER;
        if (!g_diag) return E_FAIL;
        return g_diag->QueryInterface(riid, ppvSite);
    }

    // IVisualTreeServiceCallback（回调拥有 BSTR；血泪 #5：只记录+跟踪，零引擎调用）
    STDMETHODIMP OnVisualTreeChange(ParentChildRelation relation, VisualElement element,
                                    VisualMutationType mutationType) {
        try {
            char typeA[128] = "", nameA[128] = "";
            if (element.Type) WideCharToMultiByte(CP_UTF8, 0, element.Type, -1, typeA, sizeof(typeA), NULL, NULL);
            if (element.Name) WideCharToMultiByte(CP_UTF8, 0, element.Name, -1, nameA, sizeof(nameA), NULL, NULL);
            log_line("TREE %s h=%llX parent=%llX idx=%u type=%s name=%s nchild=%u",
                     mutationType == VisualMutationType::Add ? "ADD" : "REM",
                     (unsigned long long)element.Handle, (unsigned long long)relation.Parent,
                     relation.ChildIndex, typeA, nameA, element.NumChildren);
            TrackTree(relation, element, mutationType);
            if (element.Type) SysFreeString(element.Type);
            if (element.Name) SysFreeString(element.Name);
            if (element.SrcInfo.FileName) SysFreeString(element.SrcInfo.FileName);
            if (element.SrcInfo.Hash) SysFreeString(element.SrcInfo.Hash);
            return S_OK;
        } catch (...) { return S_OK; } // 躺平
    }
    STDMETHODIMP OnElementStateChanged(InstanceHandle element, VisualElementState state,
                                       LPCWSTR context) {
        try {
            char ctxA[128] = "";
            if (context) WideCharToMultiByte(CP_UTF8, 0, context, -1, ctxA, sizeof(ctxA), NULL, NULL);
            log_line("STATE h=%llX state=%d ctx=%s", (unsigned long long)element, (int)state, ctxA);
            return S_OK;
        } catch (...) { return S_OK; }
    }
};

static void register_obj(TapObject* o) {
    AcquireSRWLockExclusive(&g_objLock); g_cur = o; ReleaseSRWLockExclusive(&g_objLock);
}
static TapObject* cur_obj() {
    AcquireSRWLockShared(&g_objLock); TapObject* o = g_cur; ReleaseSRWLockShared(&g_objLock);
    return o;
}

// ── UI 线程调度（血泪 #5：一切 XAML/引擎操作在 dispatched lambda 内执行）──
// mode: 0=读 Text；1=写 Text；2=props 链 dump（GPVC）。
// job 由 shared_ptr 持有：超时放弃等待后 lambda 仍可能晚到，绝不写已死栈帧。
struct UiJob {
    int     mode = 0;
    wchar_t text[128] = L"";          // 输入（mode1）
    wchar_t out[128] = L"";           // 输出（mode0/2：Text 值）
    wchar_t vtype[64] = L"";          // 输出（mode2：ValueType）
    unsigned idx = 0;                 // 输出（mode2：Text 属性索引）
    unsigned nprops = 0;              // 输出（mode2：属性总数）
    int     dumpRc = 0;               // 输出（mode2：0=ok）
    HRESULT hr = E_FAIL;
    LONG    done = 0;                 // 0=未跑 1=成功 2=异常
};
static int RunUiJob(std::shared_ptr<UiJob> job, DWORD timeoutMs) {
    wuc::CoreDispatcher disp{ nullptr };
    AcquireSRWLockShared(&g_stateLock); disp = g_disp; ReleaseSRWLockShared(&g_stateLock);
    if (!disp) { log_line("UIJOB no dispatcher (mode=%d)", job->mode); job->done = 3; return 2; }
    try {
        disp.RunAsync(wuc::CoreDispatcherPriority::Normal, [job]() {
            try {
                unsigned long long hTime = 0;
                AcquireSRWLockShared(&g_stateLock); hTime = g_hTime; ReleaseSRWLockShared(&g_stateLock);
                if (!hTime || !g_diag) { job->hr = E_NOT_VALID_STATE; InterlockedExchange(&job->done, 2); return; }
                wfnd::IInspectable obj = nullptr;
                HRESULT hr = g_diag->GetIInspectableFromHandle((InstanceHandle)hTime,
                    reinterpret_cast<::IInspectable**>(winrt::put_abi(obj)));
                if (FAILED(hr) || !obj) { job->hr = FAILED(hr) ? hr : E_FAIL; InterlockedExchange(&job->done, 2); return; }
                auto tb = obj.try_as<wux::Controls::TextBlock>();
                if (!tb) { job->hr = E_NOINTERFACE; InterlockedExchange(&job->done, 2); return; }
                if (job->mode == 0) {
                    wcsncpy_s(job->out, tb.Text().c_str(), _TRUNCATE);
                    job->hr = S_OK;
                } else if (job->mode == 1) {
                    tb.Text(winrt::hstring(job->text));
                    job->hr = S_OK;
                } else if (job->mode == 2) {
                    unsigned srcCount = 0, propCount = 0;
                    PropertyChainSource* srcs = nullptr; PropertyChainValue* vals = nullptr;
                    HRESULT hr2 = g_svc ? g_svc->GetPropertyValuesChain((InstanceHandle)hTime,
                        &srcCount, &srcs, &propCount, &vals) : E_FAIL;
                    if (FAILED(hr2)) { job->hr = hr2; InterlockedExchange(&job->done, 2); return; }
                    job->nprops = propCount; job->dumpRc = 3;
                    for (unsigned i = 0; i < propCount && vals; i++) {
                        PropertyChainValue& v = vals[i];
                        if (!v.PropertyName || wcscmp(v.PropertyName, L"Text")) continue;
                        char dA[96] = "", vtA[96] = "", valA[96] = "";
                        w2a(v.DeclaringType, dA, sizeof(dA)); w2a(v.ValueType, vtA, sizeof(vtA));
                        w2a(v.Value, valA, sizeof(valA));
                        log_line("PROPS entry decl=%s vtype=%s value='%s' idx=%u chainIdx=%u overridden=%d meta=0x%llx",
                                 dA, vtA, valA, v.Index, v.PropertyChainIndex, (int)v.Overridden,
                                 (long long)v.MetadataBits);
                        // 【B 实测】Text 链上有多个条目：本地 String 值（chainIdx=0）+ 上层
                        // null Object（meta=IsValueNull，值串 "0"）——只取非 null 的本地值
                        if (!(v.MetadataBits & MetadataBit::IsValueHandle)
                            && !(v.MetadataBits & MetadataBit::IsValueNull) && v.Value) {
                            wcsncpy_s(job->out, v.Value, _TRUNCATE);
                            wcsncpy_s(job->vtype, v.ValueType ? v.ValueType : L"", _TRUNCATE);
                            job->idx = v.Index;
                            job->dumpRc = 0;
                        }
                    }
                    free_chain_arrays(srcCount, srcs, propCount, vals);
                    job->hr = S_OK;
                }
                InterlockedExchange(&job->done, 1);
            } catch (const winrt::hresult_error& e) {
                job->hr = (HRESULT)e.code().value;
                InterlockedExchange(&job->done, 2);
            } catch (...) {
                job->hr = E_FAIL;
                InterlockedExchange(&job->done, 2);
            }
        });
        for (DWORD t = 0; t < timeoutMs; t += 25) {
            if (job->done) break;
            Sleep(25);
        }
        if (!job->done) {
            log_line("UIJOB timeout mode=%d (%lu ms) — abandon wait", job->mode, timeoutMs);
            return 1;
        }
        return job->done == 1 ? 0 : 2;
    } catch (const winrt::hresult_error& e) {
        log_line("UIJOB RunAsync exception hr=0x%08lx", (unsigned long)e.code().value);
        job->done = 2; job->hr = (HRESULT)e.code().value;
        return 2;
    } catch (...) {
        log_line("UIJOB RunAsync EXCEPTION");
        job->done = 2;
        return 2;
    }
}

// ── B1 命令实现（pipe 线程上下文；全部经 RunUiJob，无锁无直调）──

static bool RequireLocated(unsigned long long* hTimeOut, unsigned* genOut) {
    AcquireSRWLockShared(&g_stateLock);
    bool ok = g_located && g_hTime;
    if (ok) { *hTimeOut = g_hTime; *genOut = g_gen; }
    ReleaseSRWLockShared(&g_stateLock);
    return ok;
}

static void CmdProps() {
    unsigned long long h; unsigned gen;
    if (!RequireLocated(&h, &gen)) { send_line("{\"v\":2,\"t\":\"props\",\"err\":\"not_located\"}\n"); return; }
    auto job = std::make_shared<UiJob>(); job->mode = 2;
    int rc = RunUiJob(job, 5000);
    char a[384], t8[128], vt8[64];
    w2a(job->out, t8, sizeof(t8)); w2a(job->vtype, vt8, sizeof(vt8));
    _snprintf_s(a, sizeof(a), _TRUNCATE,
        "{\"v\":2,\"t\":\"props\",\"gen\":%u,\"job\":%d,\"rc\":%d,\"idx\":%u,\"vtype\":\"%s\",\"text\":\"%s\",\"nprops\":%u}\n",
        gen, rc, job->dumpRc, job->idx, vt8, t8, job->nprops);
    send_line(a);
}

static void CmdSet(int route, const wchar_t* text) {
    unsigned long long h; unsigned gen;
    if (!RequireLocated(&h, &gen)) { send_line("{\"v\":2,\"t\":\"setres\",\"err\":\"not_located\"}\n"); return; }
    bool already;
    AcquireSRWLockShared(&g_stateLock); already = g_snap.modified; ReleaseSRWLockShared(&g_stateLock);
    if (already) { send_line("{\"v\":2,\"t\":\"setres\",\"err\":\"already_modified\"}\n"); return; }

    // 快照原始值：mode0 读当前 Text + mode2 链 dump（属性索引）
    auto jobR = std::make_shared<UiJob>(); jobR->mode = 0;
    int rcR = RunUiJob(jobR, 5000);
    auto jobP = std::make_shared<UiJob>(); jobP->mode = 2;
    int rcP = RunUiJob(jobP, 5000);

    HRESULT hr = E_FAIL;
    if (route == 1) {
        // 路线A：GetPropertyIndex + CreateInstance("String") + SetProperty（engine 调用链，
        // 全部放 dispatched lambda 内——血泪 #5 同款纪律）
        // 实现：复用 mode3 内联在下方（避免再建一层抽象）
        wuc::CoreDispatcher disp{ nullptr };
        AcquireSRWLockShared(&g_stateLock); disp = g_disp; ReleaseSRWLockShared(&g_stateLock);
        if (!disp) {
            send_line("{\"v\":2,\"t\":\"setres\",\"err\":\"no_dispatcher\"}\n");
            return;
        }
        auto job = std::make_shared<UiJob>();
        // GPVC dump 已给出 Text 的属性索引（jobP->idx）；GetPropertyIndex 实测对该属性名
        // 返回 E_INVALIDARG（B 实测），回退用 dump 索引
        unsigned fallbackIdx = jobP->idx; bool hasFallback = (jobP->dumpRc == 0);
        try {
            disp.RunAsync(wuc::CoreDispatcherPriority::Normal, [job, h, text, fallbackIdx, hasFallback]() {
                try {
                    if (!g_svc) { job->hr = E_NOT_VALID_STATE; InterlockedExchange(&job->done, 2); return; }
                    unsigned idx = 0;
                    HRESULT hr1 = g_svc->GetPropertyIndex((InstanceHandle)h, L"Text", &idx);
                    log_line("SET-A GetPropertyIndex hr=0x%08lx idx=%u", (unsigned long)hr1, idx);
                    if (FAILED(hr1)) {
                        if (!hasFallback) { job->hr = hr1; InterlockedExchange(&job->done, 2); return; }
                        idx = fallbackIdx;
                        log_line("SET-A fallback to GPVC idx=%u", idx);
                    }
                    BSTR tName = SysAllocString(L"String");
                    BSTR tVal = SysAllocStringLen(text, (UINT)wcslen(text));
                    InstanceHandle val = 0;
                    HRESULT hr2 = g_svc->CreateInstance(tName, tVal, &val);
                    log_line("SET-A CreateInstance hr=0x%08lx val=%llX",
                             (unsigned long)hr2, (unsigned long long)val);
                    SysFreeString(tName); SysFreeString(tVal);
                    if (FAILED(hr2)) { job->hr = hr2; InterlockedExchange(&job->done, 2); return; }
                    HRESULT hr3 = g_svc->SetProperty((InstanceHandle)h, val, idx);
                    log_line("SET-A SetProperty hr=0x%08lx", (unsigned long)hr3);
                    job->idx = idx; job->hr = hr3;
                    InterlockedExchange(&job->done, SUCCEEDED(hr3) ? 1 : 2);
                } catch (...) { job->hr = E_FAIL; InterlockedExchange(&job->done, 2); }
            });
            for (DWORD t = 0; t < 5000; t += 25) { if (job->done) break; Sleep(25); }
            if (!job->done) log_line("SET-A timeout");
            hr = (job->done == 1) ? job->hr : E_FAIL;
        } catch (...) { log_line("SET-A RunAsync EXCEPTION"); hr = E_FAIL; }
    } else {
        auto job = std::make_shared<UiJob>(); job->mode = 1;
        wcsncpy_s(job->text, text, _TRUNCATE);
        int rc0 = RunUiJob(job, 5000);
        hr = (rc0 == 0) ? S_OK : E_FAIL;
        if (rc0 != 0) log_line("SET-B failed rc=%d", rc0);
    }
    if (SUCCEEDED(hr)) {
        AcquireSRWLockExclusive(&g_stateLock);
        g_snap.modified = true; g_snap.route = route; g_snap.gen = gen;
        g_snap.hasPropIdx = (jobP->dumpRc == 0); g_snap.propIdx = jobP->idx;
        g_snap.hasOrigText = (rcR == 0); wcsncpy_s(g_snap.origText, jobR->out, _TRUNCATE);
        wcsncpy_s(g_snap.written, text, _TRUNCATE);
        ReleaseSRWLockExclusive(&g_stateLock);
    }
    // 回读
    auto jobV = std::make_shared<UiJob>(); jobV->mode = 0;
    int rcV = RunUiJob(jobV, 5000);
    char a[512], o8[128], n8[128], w8[128];
    w2a(jobR->out, o8, sizeof(o8)); w2a(rcV == 0 ? jobV->out : L"", n8, sizeof(n8));
    w2a(text, w8, sizeof(w8));
    _snprintf_s(a, sizeof(a), _TRUNCATE,
        "{\"v\":2,\"t\":\"setres\",\"route\":%d,\"hr\":%lu,\"gen\":%u,\"orig\":\"%s\",\"written\":\"%s\",\"cur\":\"%s\",\"snapR\":%d,\"snapP\":%d}\n",
        route, (unsigned long)hr, gen, o8, w8, n8, rcR, jobP->dumpRc);
    send_line(a);
}

// superseded 判定：当前值 != 我们写入的值 → 系统已覆盖 → 不回写（方案 D7）
static void CmdRestore(const char* trig) {
    Snap snap;
    unsigned long long h; unsigned gen;
    bool located = RequireLocated(&h, &gen);
    AcquireSRWLockExclusive(&g_stateLock);
    snap = g_snap;
    ReleaseSRWLockExclusive(&g_stateLock);
    if (!snap.modified) {
        char a[160];
        _snprintf_s(a, sizeof(a), _TRUNCATE,
            "{\"v\":2,\"t\":\"rstres\",\"trig\":\"%s\",\"noop\":1,\"orphaned\":%d}\n",
            trig, located ? 0 : 1);
        send_line(a);
        return;
    }
    if (!located || gen != snap.gen) {
        AcquireSRWLockExclusive(&g_stateLock); g_snap = Snap{}; ReleaseSRWLockExclusive(&g_stateLock);
        log_line("RST gen mismatch (snap gen %u, now %u) — orphaned, clear only", snap.gen, gen);
        char a[160];
        _snprintf_s(a, sizeof(a), _TRUNCATE,
            "{\"v\":2,\"t\":\"rstres\",\"trig\":\"%s\",\"hr\":0,\"orphaned\":1}\n", trig);
        send_line(a);
        return;
    }
    // 读当前值判定 superseded
    auto jobV = std::make_shared<UiJob>(); jobV->mode = 0;
    int rcV = RunUiJob(jobV, 5000);
    bool superseded = (rcV == 0 && wcscmp(jobV->out, snap.written) != 0);
    HRESULT hr = S_OK;
    if (!superseded) {
        if (snap.route == 1) {
            unsigned long long h2 = h;
            wuc::CoreDispatcher disp{ nullptr };
            AcquireSRWLockShared(&g_stateLock); disp = g_disp; ReleaseSRWLockShared(&g_stateLock);
            auto job = std::make_shared<UiJob>();
            unsigned idx = snap.propIdx;
            try {
                disp.RunAsync(wuc::CoreDispatcherPriority::Normal, [job, h2, idx]() {
                    try {
                        if (!g_svc) { job->hr = E_NOT_VALID_STATE; InterlockedExchange(&job->done, 2); return; }
                        HRESULT hr2 = g_svc->ClearProperty((InstanceHandle)h2, idx);
                        log_line("RST-A ClearProperty idx=%u hr=0x%08lx", idx, (unsigned long)hr2);
                        job->hr = hr2;
                        InterlockedExchange(&job->done, SUCCEEDED(hr2) ? 1 : 2);
                    } catch (...) { job->hr = E_FAIL; InterlockedExchange(&job->done, 2); }
                });
                for (DWORD t = 0; t < 5000; t += 25) { if (job->done) break; Sleep(25); }
                hr = (job->done == 1) ? job->hr : E_FAIL;
            } catch (...) { hr = E_FAIL; }
        } else {
            auto job = std::make_shared<UiJob>(); job->mode = 1;
            wcsncpy_s(job->text, snap.hasOrigText ? snap.origText : L"", _TRUNCATE);
            int rc0 = RunUiJob(job, 5000);
            hr = (rc0 == 0) ? S_OK : E_FAIL;
        }
    } else {
        log_line("RST superseded: cur != written — leave system value in place");
    }
    AcquireSRWLockExclusive(&g_stateLock); g_snap = Snap{}; ReleaseSRWLockExclusive(&g_stateLock);
    auto jobF = std::make_shared<UiJob>(); jobF->mode = 0;
    int rcF = RunUiJob(jobF, 5000);
    char a[448], n8[128], f8[128];
    w2a(rcV == 0 ? jobV->out : L"", n8, sizeof(n8));
    w2a(rcF == 0 ? jobF->out : L"", f8, sizeof(f8));
    _snprintf_s(a, sizeof(a), _TRUNCATE,
        "{\"v\":2,\"t\":\"rstres\",\"trig\":\"%s\",\"route\":%d,\"hr\":%lu,\"superseded\":%d,\"cur\":\"%s\",\"final\":\"%s\"}\n",
        trig, snap.route, (unsigned long)hr, (int)superseded, n8, f8);
    send_line(a);
}

// ── C0 探针（v31+）：插入通道/宽度约束实测（方案 §5 C0）──────────────
// 纪律（血泪 #5 沿用）：一切 winRT/引擎操作在 dispatched lambda 内；本组函数只在
// pipe 线程组包与等 ack。全部只读或「一次可逆插入+删除」，带代次令牌。
static wfnd::IInspectable g_probeObj{ nullptr };  // C0 插入探针元素（strong ref，仅 UI 线程触碰）
static volatile LONG      g_probeOn = 0;          // 探针在场标志（跨线程可见，stats 用）
static unsigned           g_probeGen = 0;

static bool RequireHandles(unsigned long long* hTime, unsigned long long* hStack,
                           unsigned long long* hCont, unsigned long long* hDTIC, unsigned* genOut) {
    AcquireSRWLockShared(&g_stateLock);
    bool ok = g_located && g_hTime && g_hStack && g_hCont && g_hDTIC;
    if (ok) {
        *hTime = g_hTime; *hStack = g_hStack; *hCont = g_hCont; *hDTIC = g_hDTIC; *genOut = g_gen;
    }
    ReleaseSRWLockShared(&g_stateLock);
    return ok;
}

static void FwDump(const char* tag, int depth, wfnd::IInspectable const& o) {
    char clsA[128] = "", nmA[96] = "";
    try { w2a(winrt::get_class_name(o).c_str(), clsA, sizeof(clsA)); }
    catch (...) { strcpy_s(clsA, "?"); }
    auto fe = o.try_as<wux::FrameworkElement>();
    if (!fe) { log_line("%s d=%d cls=%s (no FW)", tag, depth, clsA); return; }
    try { w2a(fe.Name().c_str(), nmA, sizeof(nmA)); } catch (...) {}
    wux::Thickness mg{ 0,0,0,0 };
    try { mg = fe.Margin(); } catch (...) {}
    log_line("%s d=%d cls=%s name=%s act=%.1fx%.1f w=%.0f h=%.0f minw=%.0f maxw=%.0f margin=[%.0f %.0f %.0f %.0f] hal=%d val=%d",
             tag, depth, clsA, nmA, fe.ActualWidth(), fe.ActualHeight(), fe.Width(), fe.Height(),
             fe.MinWidth(), fe.MaxWidth(), mg.Left, mg.Top, mg.Right, mg.Bottom,
             (int)fe.HorizontalAlignment(), (int)fe.VerticalAlignment());
    try {
        if (auto sp = o.try_as<wux::Controls::StackPanel>())
            log_line("%s   stackpanel orientation=%d", tag, (int)sp.Orientation());
    } catch (...) {}
    try {
        if (auto grid = o.try_as<wux::Controls::Grid>()) {
            // 直接量子元素布局槽（实际分配宽度比列定义更接近实测需求；SDK cppwinrt
            // 头的 ColumnDefinition::Width 在本版本报 C2064，弃用）
            auto ch = grid.Children();
            for (uint32_t i = 0; i < ch.Size() && i < 12; i++) {
                if (auto cf = ch.GetAt(i).try_as<wux::FrameworkElement>()) {
                    auto slot = wux::Controls::Primitives::LayoutInformation::GetLayoutSlot(cf);
                    log_line("%s   gridchild[%u] act=%.1f slot=[%.1f %.1f %.1f %.1f]",
                             tag, i, cf.ActualWidth(), slot.X, slot.Y, slot.Width, slot.Height);
                }
            }
        }
    } catch (...) {}
}

static void DumpTextBlockStyle(wux::Controls::TextBlock const& tb, const char* which) {
    try {
        char ffA[96] = "", fgA[96] = "";
        w2a(tb.FontFamily().Source().c_str(), ffA, sizeof(ffA));
        auto fg = tb.Foreground();
        if (auto scb = fg.try_as<wuxm::SolidColorBrush>()) {
            auto c = scb.Color();
            _snprintf_s(fgA, sizeof(fgA), _TRUNCATE, "ARGB=%02X%02X%02X%02X", c.A, c.R, c.G, c.B);
        } else if (fg) { w2a(winrt::get_class_name(fg).c_str(), fgA, sizeof(fgA)); }
        wux::Thickness mg{0,0,0,0}, pd{0,0,0,0};
        try { mg = tb.Margin(); } catch (...) {}
        try { pd = tb.Padding(); } catch (...) {}
        log_line("C0STYLE %s fontsize=%.2f family=%s weight=%d foreground=%s margin=[%.1f %.1f %.1f %.1f] padding=[%.1f %.1f %.1f %.1f] textalign=%d charspace=%d lineheight=%.1f wrap=%d trim=%d",
                 which, tb.FontSize(), ffA, (int)tb.FontWeight().Weight, fgA,
                 mg.Left, mg.Top, mg.Right, mg.Bottom, pd.Left, pd.Top, pd.Right, pd.Bottom,
                 (int)tb.TextAlignment(), tb.CharacterSpacing(), tb.LineHeight(),
                 (int)tb.TextWrapping(), (int)tb.TextTrimming());
    } catch (...) { log_line("C0STYLE %s dump exception", which); }
}

// mode: 3=c0tree 4=c0ins 5=c0meas 6=c0rm 7=c0add（引擎结构通道对照）
static int RunC0Job(std::shared_ptr<UiJob> job, DWORD timeoutMs) {
    wuc::CoreDispatcher disp{ nullptr };
    AcquireSRWLockShared(&g_stateLock); disp = g_disp; ReleaseSRWLockShared(&g_stateLock);
    if (!disp) { log_line("C0 no dispatcher (mode=%d)", job->mode); job->done = 3; return 2; }
    unsigned long long hTime = 0, hStack = 0, hCont = 0, hDTIC = 0; unsigned gen = 0;
    bool located = RequireHandles(&hTime, &hStack, &hCont, &hDTIC, &gen);
    try {
        disp.RunAsync(wuc::CoreDispatcherPriority::Normal,
            [job, located, hTime, hStack, hCont, hDTIC, gen]() {
            try {
                if (!g_diag) { job->hr = E_NOT_VALID_STATE; InterlockedExchange(&job->done, 2); return; }
                auto getObj = [](unsigned long long h) -> wfnd::IInspectable {
                    wfnd::IInspectable o{ nullptr };
                    if (h && g_diag) {
                        HRESULT hr = g_diag->GetIInspectableFromHandle((InstanceHandle)h,
                            reinterpret_cast<::IInspectable**>(winrt::put_abi(o)));
                        if (FAILED(hr)) o = nullptr;
                    }
                    return o;
                };
                if (job->mode == 3) { // c0tree：只读祖先链 + 样式 + 布局槽
                    if (!located) { job->hr = E_NOT_VALID_STATE; InterlockedExchange(&job->done, 2); return; }
                    auto tb = getObj(hTime).try_as<wux::Controls::TextBlock>();
                    if (!tb) { job->hr = E_NOINTERFACE; InterlockedExchange(&job->done, 2); return; }
                    DumpTextBlockStyle(tb, "time");
                    auto spIns = getObj(hStack);
                    if (auto sp = spIns.try_as<wux::Controls::StackPanel>()) {
                        auto ch = sp.Children();
                        log_line("C0TREE stack children=%u", ch.Size());
                        for (uint32_t i = 0; i < ch.Size(); i++) {
                            auto c = ch.GetAt(i);
                            char clsA[128] = "", nmA[96] = "";
                            try { w2a(winrt::get_class_name(c).c_str(), clsA, sizeof(clsA)); } catch (...) {}
                            if (auto f2 = c.try_as<wux::FrameworkElement>())
                                try { w2a(f2.Name().c_str(), nmA, sizeof(nmA)); } catch (...) {}
                            log_line("C0TREE stack child[%u] cls=%s name=%s", i, clsA, nmA);
                            if (auto t2 = c.try_as<wux::Controls::TextBlock>())
                                DumpTextBlockStyle(t2, nmA[0] ? nmA : "other");
                        }
                    }
                    wux::DependencyObject cur = tb;
                    for (int d = 0; d < 14; d++) {
                        FwDump("C0TREE", d, cur.as<wfnd::IInspectable>());
                        auto parent = wuxm::VisualTreeHelper::GetParent(cur);
                        if (!parent) break;
                        cur = parent;
                    }
                    auto cIns = getObj(hCont);
                    if (auto grid = cIns.try_as<wux::Controls::Grid>()) {
                        auto ch = grid.Children();
                        for (uint32_t i = 0; i < ch.Size(); i++) {
                            if (auto f2 = ch.GetAt(i).try_as<wux::FrameworkElement>()) {
                                char clsA[128] = "", nmA[96] = "";
                                try { w2a(winrt::get_class_name(f2).c_str(), clsA, sizeof(clsA)); } catch (...) {}
                                try { w2a(f2.Name().c_str(), nmA, sizeof(nmA)); } catch (...) {}
                                try {
                                    auto slot = wux::Controls::Primitives::LayoutInformation::GetLayoutSlot(f2);
                                    log_line("C0TREE cont child[%u] cls=%s name=%s slot=[%.1f %.1f %.1f %.1f]",
                                             i, clsA, nmA, slot.X, slot.Y, slot.Width, slot.Height);
                                } catch (...) {
                                    log_line("C0TREE cont child[%u] cls=%s name=%s slot=<exc>", i, clsA, nmA);
                                }
                            }
                        }
                    }
                    job->hr = S_OK; InterlockedExchange(&job->done, 1); return;
                }
                if (job->mode == 4) { // c0ins：winRT 插入探针（路线 B 结构形态）
                    if (!located) { job->hr = E_NOT_VALID_STATE; InterlockedExchange(&job->done, 2); return; }
                    if (g_probeObj) {
                        log_line("C0INS probe already present (gen %u) — skip", g_probeGen);
                        job->hr = S_OK; InterlockedExchange(&job->done, 1); return;
                    }
                    auto sp = getObj(hStack).try_as<wux::Controls::StackPanel>();
                    auto tb = getObj(hTime).try_as<wux::Controls::TextBlock>();
                    if (!sp || !tb) { job->hr = E_NOINTERFACE; InterlockedExchange(&job->done, 2); return; }
                    double before = sp.ActualWidth();
                    unsigned cntBefore = sp.Children().Size();
                    wux::Controls::TextBlock probe;
                    probe.Text(L"C0PROBE");
                    try { probe.FontSize(tb.FontSize()); } catch (...) {}
                    try { probe.FontFamily(tb.FontFamily()); } catch (...) {}
                    try { probe.FontWeight(tb.FontWeight()); } catch (...) {}
                    try { probe.Foreground(tb.Foreground()); } catch (...) {}
                    probe.VerticalAlignment(wux::VerticalAlignment::Center);
                    probe.Margin({ 8,0,0,0 });
                    probe.TextWrapping(wux::TextWrapping::NoWrap);
                    sp.Children().InsertAt(0, probe);
                    g_probeObj = probe; g_probeGen = gen;
                    InterlockedExchange(&g_probeOn, 1);
                    log_line("C0INS inserted idx=0 cnt %u->%u actW(before)=%.1f gen=%u",
                             cntBefore, sp.Children().Size(), before, gen);
                    job->hr = S_OK; InterlockedExchange(&job->done, 1); return;
                }
                if (job->mode == 5) { // c0meas：几何量测（插入前后对比）
                    if (!located) { job->hr = E_NOT_VALID_STATE; InterlockedExchange(&job->done, 2); return; }
                    const unsigned long long hs[4] = { hDTIC, hCont, hStack, hTime };
                    const char* nm[4] = { "DTIC", "Cont", "Stack", "Time" };
                    for (int i = 0; i < 4; i++) {
                        if (auto f = getObj(hs[i]).try_as<wux::FrameworkElement>()) {
                            auto ds = f.DesiredSize();
                            log_line("C0MEAS %s act=%.1fx%.1f desired=%.1fx%.1f", nm[i],
                                     f.ActualWidth(), f.ActualHeight(), ds.Width, ds.Height);
                        }
                    }
                    if (g_probeObj) {
                        if (auto pf = g_probeObj.try_as<wux::FrameworkElement>()) {
                            auto ds = pf.DesiredSize();
                            log_line("C0MEAS probe act=%.1fx%.1f desired=%.1fx%.1f",
                                     pf.ActualWidth(), pf.ActualHeight(), ds.Width, ds.Height);
                        }
                    }
                    if (auto sp = getObj(hStack).try_as<wux::Controls::StackPanel>()) {
                        auto ch = sp.Children();
                        for (uint32_t i = 0; i < ch.Size(); i++) {
                            if (auto f2 = ch.GetAt(i).try_as<wux::FrameworkElement>()) {
                                char clsA[128] = "", nmA[96] = "";
                                try { w2a(winrt::get_class_name(f2).c_str(), clsA, sizeof(clsA)); } catch (...) {}
                                try { w2a(f2.Name().c_str(), nmA, sizeof(nmA)); } catch (...) {}
                                auto ds = f2.DesiredSize();
                                log_line("C0MEAS stack child[%u] cls=%s name=%s act=%.1f desired=%.1f",
                                         i, clsA, nmA, f2.ActualWidth(), ds.Width);
                            }
                        }
                    }
                    job->hr = S_OK; InterlockedExchange(&job->done, 1); return;
                }
                if (job->mode == 6) { // c0rm：删除探针（可逆性）
                    if (!g_probeObj) { log_line("C0RM no probe present"); job->hr = S_OK; InterlockedExchange(&job->done, 1); return; }
                    auto fe = g_probeObj.try_as<wux::FrameworkElement>();
                    if (!fe) { g_probeObj = nullptr; InterlockedExchange(&g_probeOn, 0);
                        log_line("C0RM probe ref dead — cleared"); job->hr = S_OK; InterlockedExchange(&job->done, 1); return; }
                    auto parent = wuxm::VisualTreeHelper::GetParent(fe);
                    auto panel = parent ? parent.try_as<wux::Controls::Panel>() : nullptr;
                    if (!panel) {
                        log_line("C0RM probe orphaned (parent gone; ins gen=%u now=%u) — clear only", g_probeGen, gen);
                        g_probeObj = nullptr; InterlockedExchange(&g_probeOn, 0);
                        job->hr = S_OK; InterlockedExchange(&job->done, 1); return;
                    }
                    auto ch = panel.Children();
                    int idx = -1;
                    auto feU = fe.try_as<wux::UIElement>();
                    for (uint32_t i = 0; i < ch.Size(); i++) {
                        if (feU && winrt::get_abi(ch.GetAt(i)) == winrt::get_abi(feU)) { idx = (int)i; break; }
                    }
                    if (idx < 0) {
                        log_line("C0RM probe not in parent children (already removed) — clear");
                        g_probeObj = nullptr; InterlockedExchange(&g_probeOn, 0);
                        job->hr = S_OK; InterlockedExchange(&job->done, 1); return;
                    }
                    ch.RemoveAt((uint32_t)idx);
                    g_probeObj = nullptr; InterlockedExchange(&g_probeOn, 0);
                    log_line("C0RM removed idx=%d cnt now=%u", idx, ch.Size());
                    job->hr = S_OK; InterlockedExchange(&job->done, 1); return;
                }
                if (job->mode == 7) { // c0add：引擎结构通道对照（AddChild 是否触达真树）
                    if (!g_svc || !located) { job->hr = E_NOT_VALID_STATE; InterlockedExchange(&job->done, 2); return; }
                    auto sp = getObj(hStack).try_as<wux::Controls::StackPanel>();
                    if (!sp) { job->hr = E_NOINTERFACE; InterlockedExchange(&job->done, 2); return; }
                    unsigned engBefore = 0;
                    g_svc->GetCollectionCount((InstanceHandle)hStack, &engBefore);
                    unsigned rtBefore = sp.Children().Size();
                    log_line("C0ADD before eng=%u rt=%u", engBefore, rtBefore);
                    BSTR tn = SysAllocString(L"Windows.UI.Xaml.Controls.TextBlock");
                    BSTR tv = SysAllocString(L"");
                    InstanceHandle hChild = 0;
                    HRESULT hrC = tn && tv ? g_svc->CreateInstance(tn, tv, &hChild) : E_OUTOFMEMORY;
                    if (tn) SysFreeString(tn);
                    if (tv) SysFreeString(tv);
                    log_line("C0ADD CreateInstance(TextBlock) hr=0x%08lx h=%llX",
                             (unsigned long)hrC, (unsigned long long)hChild);
                    if (SUCCEEDED(hrC) && hChild) {
                        HRESULT hrA = g_svc->AddChild((InstanceHandle)hStack, hChild, 0);
                        log_line("C0ADD AddChild hr=0x%08lx", (unsigned long)hrA);
                        unsigned engAfter = 0; g_svc->GetCollectionCount((InstanceHandle)hStack, &engAfter);
                        unsigned rtAfter = sp.Children().Size();
                        log_line("C0ADD after eng=%u rt=%u ==> %s", engAfter, rtAfter,
                                 (engAfter == engBefore + 1 && rtAfter == rtBefore)
                                     ? "engine-model-only (NOT rendered; route B needed)"
                                 : (rtAfter == rtBefore + 1) ? "REAL TREE MUTATED (render expected)"
                                                             : "other");
                        if (SUCCEEDED(hrA)) {
                            HRESULT hrR = g_svc->RemoveChild((InstanceHandle)hStack, 0);
                            unsigned engFix = 0; g_svc->GetCollectionCount((InstanceHandle)hStack, &engFix);
                            log_line("C0ADD RemoveChild hr=0x%08lx eng now=%u (revert)", (unsigned long)hrR, engFix);
                        }
                    }
                    job->hr = S_OK; InterlockedExchange(&job->done, 1); return;
                }
                job->hr = E_INVALIDARG; InterlockedExchange(&job->done, 2);
            } catch (const winrt::hresult_error& e) {
                job->hr = (HRESULT)e.code().value;
                InterlockedExchange(&job->done, 2);
            } catch (...) {
                job->hr = E_FAIL;
                InterlockedExchange(&job->done, 2);
            }
        });
        for (DWORD t = 0; t < timeoutMs; t += 25) {
            if (job->done) break;
            Sleep(25);
        }
        if (!job->done) {
            log_line("C0 timeout mode=%d (%lu ms)", job->mode, timeoutMs);
            return 1;
        }
        return job->done == 1 ? 0 : 2;
    } catch (const winrt::hresult_error& e) {
        log_line("C0 RunAsync exception hr=0x%08lx", (unsigned long)e.code().value);
        job->done = 2; job->hr = (HRESULT)e.code().value;
        return 2;
    } catch (...) {
        log_line("C0 RunAsync EXCEPTION");
        job->done = 2;
        return 2;
    }
}

static void CmdC0(int mode, const char* tag) {
    unsigned long long h; unsigned gen;
    if (!RequireLocated(&h, &gen)) { // 尚未定位：报告而非执行
        char a[128];
        _snprintf_s(a, sizeof(a), _TRUNCATE, "{\"v\":2,\"t\":\"%s\",\"rc\":9,\"err\":\"not_located\"}\n", tag);
        send_line(a);
        return;
    }
    auto job = std::make_shared<UiJob>(); job->mode = mode;
    int rc = RunC0Job(job, 6000);
    char a[128];
    _snprintf_s(a, sizeof(a), _TRUNCATE, "{\"v\":2,\"t\":\"%s\",\"rc\":%d,\"hr\":%lu}\n",
                tag, rc, (unsigned long)job->hr);
    send_line(a);
}

// ── C1 静态五段面板 + C2 输入拦截（v34 重设计；方案 §5 C 行 / D3 宽度定案）────
// v33 教训（实测）：直接把原生 StackPanel 改横向会与系统每秒的 Vertical 回写形成
// 属性拉锯；且 01:07:02 explorer AppHangB1 恰在主题翻转+拉锯期间（归因未定案，
// 拉锯是头号在案嫌疑）。v34 根则：**对原生元素零布局属性写入**——
//   建自建横向 StackPanel，把原生 TimeInnerTextBlock reparent 进来（系统 VM 每秒
//   仍按引用写 Time.Text，与父无关）；自建面板作为整体插在原生 StackPanel 第 0 位；
//   原生面板保持 Vertical（其子=[自建横板, Date(隐藏)]，视觉上恰为一行）。
//   Date 隐藏=一次性写入（v33 实证系统不复活）；输入 Border=ContainerGrid 追加。
// 持续持有 = 1s 维护定时器：校验 Time 在自建面板内/Date 隐藏/自建面板在场/border
// 在场/**僵尸检测**（GetParent(原生sp)==null ⇒ 树被静默重建而引擎零事件——v33 实测
// 主题翻转可触发；此时 Unadvise+Advise 强制全树重放→重定位→自动重建）。
// 宿主心跳兜底（§4.2）：pipe 静默 >35s 视同断线，自动恢复。
struct PanelStyle {
    double  fontscale = 0.55;   // 段字号 = Time.FontSize * fontscale
    double  segmaxw = 170;      // 单段 MaxWidth（逻辑 px），超出省略号截断
    double  capw = 620;         // 自建横板 MaxWidth（显式宽度管理，D3）
    int     input = 1;          // C2 输入拦截 Border 开关
};
static PanelStyle         g_style;
static wchar_t            g_segText[4][64];        // 天气 节日 节气 农历（host 下发缓存）
static unsigned           g_panelGen = 0;          // 建立时代次令牌
static wfnd::IInspectable g_seg[4]{ nullptr, nullptr, nullptr, nullptr };
static wfnd::IInspectable g_hpanelRef{ nullptr };   // 自建横板（含 4 段+Time）
static wfnd::IInspectable g_timeRef{ nullptr };     // 原生 Time（被 reparent，VM 持续写其 Text）
static wfnd::IInspectable g_spRef{ nullptr }, g_dateRef{ nullptr }, g_contRef{ nullptr };
static double             g_segDesired[4] = { 0,0,0,0 };
static bool               g_segHidden[4] = { false,false,false,false };
static int                g_snapDateVisibility = -1;  // -1=未改
static int                g_snapTimeIndex = -1;       // Time 在原生面板的原位（恢复用）
static winrt::event_token g_szToken{}, g_themeToken{};
static bool               g_eventsOn = false;
static wux::DispatcherTimer g_timer{ nullptr };
static wfnd::IInspectable g_borderRef{ nullptr };
static volatile LONG      g_lastRecvTick = 0;      // pipe 最近收到命令的时刻（GetTickCount）
static volatile LONG      g_lastResyncTick = 0;    // 上次僵尸重同步时刻（限频 30s）
// 隐藏优先级：天气(0)→节日(1)→节气(2)→农历(3)；时间段=原生 Time 永不丢
static const char* SEGNAME[4] = { "weather", "festival", "term", "lunar" };
static void AutoRestoreOnDisconnect(); // 定义于后（B0 恢复序列）
static void PanelReflow(wux::Controls::StackPanel const& hp); // 定义于后（v41 tick 兜底调用）

// v35 正向判据：Time 的现父是否为自建横板（UI 线程；winRT 调用非引擎调用）
static bool SelfReparentPositive() {
    try {
        if (!g_panelOn || !g_timeRef || !g_hpanelRef) return false;
        auto t = g_timeRef.try_as<wux::DependencyObject>();
        auto hp = g_hpanelRef.try_as<wux::DependencyObject>();
        if (!t || !hp) return false;
        auto p = wuxm::VisualTreeHelper::GetParent(t);
        return p && winrt::get_abi(p.as<wfnd::IInspectable>()) == winrt::get_abi(hp.as<wfnd::IInspectable>());
    } catch (...) { return false; }
}

// 树重建（gen++）时由 TrackTree 在 UI 线程调用：只置 panelOn=0（引用保留——断线时
// PanelFree 仍需引用来拆除已 orphan 的面板元素，否则残留永远在屏上）。
// 维护 tick/重建由 gen 令牌失配自然灭活。
static void PanelOrphan() {
    for (int i = 0; i < 4; i++) { g_segHidden[i] = false; }
    g_eventsOn = false;
    InterlockedExchange(&g_panelOn, 0);
}

// v35：reparent 自我 REM 抑制的置位窗口见 PanelBuild（RemoveAt 前后）；
// 正向校验 SelfReparentPositive 已随前置声明区定义。

static void TouchRecv() { InterlockedExchange(&g_lastRecvTick, (LONG)GetTickCount()); }

// 宿主心跳兜底：pipe 静默超时 → 视同断线走恢复序列。
// 由 PanelTick（UI 线程，1s 一次）调用；breach 时开工作线程执行——AutoRestoreOnDisconnect
// 内部有 RunUiJob 睡眠等待，绝不能在 UI 线程跑。
static void HeartbeatCheck() {
    LONG last = g_lastRecvTick;
    if (!last) return;
    LONG now = (LONG)GetTickCount();
    LONG dt = now - last; // GetTickCount 49 天回绕，C 阶段会话时长内忽略
    if (dt < 0) dt = -dt;
    if (dt > 35000) {
        InterlockedExchange(&g_lastRecvTick, now); // 防重复触发
        log_line("HEARTBEAT pipe silent %ld ms — treat as dead host (worker)", dt);
        HANDLE t = CreateThread(NULL, 0, [](LPVOID) -> DWORD {
            try { AutoRestoreOnDisconnect(); } catch (...) { log_line("HEARTBEAT AUTO EXCEPTION"); }
            return 0;
        }, NULL, 0, NULL);
        if (t) CloseHandle(t);
    }
}

// 极简有界 JSON（协议 §4.3：DLL 内限界解析）：只认 "key":"value"（值禁转义/控制符）
// 与 "key":number；总长与值长受限；任何异常→整条丢弃（host 收 ack err）。
static bool JGetStr(const char* j, const char* key, wchar_t* out, size_t nOut) {
    char pat[40];
    _snprintf_s(pat, sizeof(pat), _TRUNCATE, "\"%s\":\"", key);
    const char* p = strstr(j, pat);
    if (!p) return false;
    p += strlen(pat);
    char val[180]; size_t n = 0;
    while (*p && *p != '"' && n < sizeof(val) - 1) {
        if (*p == '\\' || (unsigned char)*p < 0x20) return false;
        val[n++] = *p++;
    }
    if (*p != '"' || n == 0) return false;
    val[n] = 0;
    return MultiByteToWideChar(CP_UTF8, 0, val, -1, out, (int)nOut) > 0;
}
static bool JGetDbl(const char* j, const char* key, double* out) {
    char pat[40];
    _snprintf_s(pat, sizeof(pat), _TRUNCATE, "\"%s\":", key);
    const char* p = strstr(j, pat);
    if (!p) return false;
    p += strlen(pat);
    char* end = nullptr;
    double v = strtod(p, &end);
    if (end == p || v < 0 || v > 10000) return false;
    *out = v;
    return true;
}

static void SendTapEvent(const char* button, double x, double y);

// tap 事件发送线程（v36）：UI 线程只入队——v35 实测 UI 线程直接 WriteFile 管道会被
// 卡 ~9s（与 pipe 线程的阻塞 ReadFile 形成 interlock，血泪 #6 变体），点一下时钟=
// 任务栏冻结 9 秒，绝不可接受。事件频率=人工点击量级，有界环形队列 16 条足够。
static SRWLOCK g_tapQLock = SRWLOCK_INIT;
static char    g_tapQ[16][160];
static int     g_tapQHead = 0, g_tapQTail = 0;
static HANDLE  g_tapQEvent = NULL;

static void TapEnqueue(const char* msg) {
    AcquireSRWLockExclusive(&g_tapQLock);
    int next = (g_tapQHead + 1) % 16;
    if (next != g_tapQTail) {
        strcpy_s(g_tapQ[g_tapQHead], msg);
        g_tapQHead = next;
    } // 队列满=丢弃最旧策略省略（点击量级不会触顶），直接丢弃新事件并计数
    ReleaseSRWLockExclusive(&g_tapQLock);
    if (g_tapQEvent) SetEvent(g_tapQEvent);
}

static DWORD WINAPI tapq_thread(LPVOID) {
    for (;;) {
        WaitForSingleObject(g_tapQEvent, INFINITE);
        for (;;) {
            char msg[160];
            bool have = false;
            AcquireSRWLockExclusive(&g_tapQLock);
            if (g_tapQTail != g_tapQHead) {
                strcpy_s(msg, g_tapQ[g_tapQTail]);
                g_tapQTail = (g_tapQTail + 1) % 16;
                have = true;
            }
            ReleaseSRWLockExclusive(&g_tapQLock);
            if (!have) break;
            send_line(msg);
        }
    }
}

static void EnsureTapQThread() {
    static LONG started = 0;
    if (!InterlockedExchange(&started, 1)) {
        g_tapQEvent = CreateEventW(NULL, FALSE, FALSE, NULL);
        CreateThread(NULL, 0, tapq_thread, NULL, 0, NULL);
    }
}

static void SendTapEvent(const char* button, double x, double y) {
    unsigned gen = 0;
    AcquireSRWLockShared(&g_stateLock); gen = g_gen; ReleaseSRWLockShared(&g_stateLock);
    char out[160];
    _snprintf_s(out, sizeof(out), _TRUNCATE,
        "{\"v\":2,\"t\":\"tap\",\"button\":\"%s\",\"x\":%.0f,\"y\":%.0f,\"gen\":%u}\n",
        button, x, y, gen);
    EnsureTapQThread();
    TapEnqueue(out); // UI 线程零管道 I/O
}

// 维护定时器 Tick（UI 线程）：v34 结构校验（自建面板在场/Time 在自建面板内/Date 隐藏/
// border 在场）+ 僵尸检测（原生 sp 脱离视觉树 ⇒ 引擎全盲的静默重建）。
static void PanelTick() {
    try {
        if (!g_panelOn) return;
        HeartbeatCheck();
        unsigned gen = 0;
        AcquireSRWLockShared(&g_stateLock); gen = g_gen; ReleaseSRWLockShared(&g_stateLock);
        if (gen != g_panelGen) return;
        auto sp = g_spRef ? g_spRef.try_as<wux::Controls::StackPanel>() : nullptr;
        if (!sp) return;
        // 僵尸检测：原生 sp 的父为 null ⇒ 已脱离视觉树（v33 实测：主题翻转可静默重建
        // 任务栏树且引擎零事件）。限频 30s 触发 Unadvise+Advise 全树重放。
        auto spParent = wuxm::VisualTreeHelper::GetParent(sp);
        if (!spParent) {
            LONG now = (LONG)GetTickCount(), last = g_lastResyncTick;
            LONG dt = now - last; if (dt < 0) dt = -dt;
            if (dt > 30000) {
                InterlockedExchange(&g_lastResyncTick, now);
                log_line("PANEL tick: ZOMBIE detected (native sp detached) — resync advise");
                PanelOrphan();
                TapObject* o = cur_obj();
                if (o) {
                    o->UnadvisePublic();
                    o->AdvisePublic();  // 独立线程全树重放 → lost/locate → 自动重建
                }
            }
            return;
        }
        unsigned fixed = 0;
        // Date 隐藏保持
        if (g_dateRef) {
            if (auto d = g_dateRef.try_as<wux::FrameworkElement>()) {
                if (d.Visibility() != wux::Visibility::Collapsed) {
                    d.Visibility(wux::Visibility::Collapsed);
                    fixed++;
                    log_line("PANEL tick: re-hide Date (system resurrect)");
                }
            }
        }
        // 自建横板在场校验（在原生 sp 中）
        auto spCh = sp.Children();
        auto hp = g_hpanelRef.try_as<wux::Controls::StackPanel>();
        if (hp) {
            auto hpU = g_hpanelRef.try_as<wux::UIElement>();
            bool present = false;
            for (uint32_t c = 0; c < spCh.Size(); c++)
                if (hpU && winrt::get_abi(spCh.GetAt(c)) == winrt::get_abi(hpU)) { present = true; break; }
            if (!present) {
                spCh.InsertAt(0, g_hpanelRef.try_as<wux::UIElement>());
                fixed++;
                log_line("PANEL tick: re-insert hpanel at 0");
            }
        }
        // Time 在自建横板内校验（系统重主题化可能把它放回原生面板）
        if (hp && g_timeRef) {
            auto timeU = g_timeRef.try_as<wux::UIElement>();
            auto hpCh = hp.Children();
            bool inHpanel = false;
            for (uint32_t c = 0; c < hpCh.Size(); c++)
                if (timeU && winrt::get_abi(hpCh.GetAt(c)) == winrt::get_abi(timeU)) { inHpanel = true; break; }
            if (!inHpanel) {
                auto curParent = wuxm::VisualTreeHelper::GetParent(g_timeRef.as<wux::DependencyObject>());
                if (curParent) {
                    if (auto pp = curParent.try_as<wux::Controls::Panel>()) {
                        auto pch = pp.Children();
                        for (uint32_t c = 0; c < pch.Size(); c++)
                            if (timeU && winrt::get_abi(pch.GetAt(c)) == winrt::get_abi(timeU)) { pch.RemoveAt(c); break; }
                    }
                }
                hp.Children().Append(g_timeRef.try_as<wux::UIElement>());
                fixed++;
                log_line("PANEL tick: re-reparent Time into hpanel");
            }
        }
        // 段成员校验（在自建横板内；Time 必须在最后——段都在它前面）
        if (hp) {
            auto hpCh = hp.Children();
            auto timeU = g_timeRef.try_as<wux::UIElement>();
            for (int i = 0; i < 4; i++) {
                if (!g_seg[i]) continue;
                auto ui = g_seg[i].try_as<wux::UIElement>();
                bool present = false;
                for (uint32_t c = 0; c < hpCh.Size(); c++)
                    if (ui && winrt::get_abi(hpCh.GetAt(c)) == winrt::get_abi(ui)) { present = true; break; }
                if (!present) {
                    uint32_t at = 0; // 段插到 Time 之前
                    for (uint32_t c = 0; c < hpCh.Size(); c++) {
                        if (timeU && winrt::get_abi(hpCh.GetAt(c)) == winrt::get_abi(timeU)) { at = c; break; }
                    }
                    if (at > (uint32_t)i) at = (uint32_t)i;
                    hpCh.InsertAt(at, g_seg[i].try_as<wux::UIElement>());
                    fixed++;
                    log_line("PANEL tick: re-insert seg %s at %u", SEGNAME[i], (unsigned)at);
                }
            }
        }
        // border 在场校验（ContainerGrid 内）
        if (g_borderRef && g_contRef) {
            if (auto cg = g_contRef.try_as<wux::Controls::Grid>()) {
                auto cgCh = cg.Children();
                auto bu = g_borderRef.try_as<wux::UIElement>();
                bool present = false;
                for (uint32_t c = 0; c < cgCh.Size(); c++)
                    if (bu && winrt::get_abi(cgCh.GetAt(c)) == winrt::get_abi(bu)) { present = true; break; }
                if (!present) {
                    cg.Children().Append(bu);
                    fixed++;
                    log_line("PANEL tick: re-append input border");
                }
            }
        }
        // 缓存可见段期望宽（截断恢复判据）
        for (int i = 0; i < 4; i++) {
            if (!g_seg[i] || g_segHidden[i]) continue;
            if (auto f = g_seg[i].try_as<wux::FrameworkElement>()) {
                double dw = f.DesiredSize().Width;
                if (dw > 0) g_segDesired[i] = dw;
            }
        }
        if (fixed) log_line("PANEL tick: fixed=%u", fixed);
        // 宽度策略兜底：SizeChanged 漏掉的场景（如 c1set 改 capw）每秒复核一次
        if (auto hp = g_hpanelRef.try_as<wux::Controls::StackPanel>()) PanelReflow(hp);
    } catch (const winrt::hresult_error& e) {
        log_line("PANEL tick hresult hr=0x%08lx — lie flat", (unsigned long)e.code().value);
    } catch (...) {
        log_line("PANEL tick EXCEPTION — lie flat");
    }
}

// 宽度策略（D3）：溢出→按优先级隐藏段；富余→按优先级恢复段。
// v42 实测修正：MaxWidth 会钳平 DesiredSize，「desired−actual」测不到上限截断；
// 正确度量 = 可见子项期望宽之和（含 Time 与段间距） − 横板实际宽。
static void PanelReflow(wux::Controls::StackPanel const& hp) {
    try {
        if (!g_panelOn) return;
        double sum = 0;
        for (int i = 0; i < 4; i++) {
            if (!g_seg[i] || g_segHidden[i]) continue;
            if (auto f = g_seg[i].try_as<wux::FrameworkElement>()) {
                double d = f.DesiredSize().Width;
                if (d > 0) { sum += d + (i > 0 ? 10 : 0); }
            }
        }
        if (g_timeRef)
            if (auto t = g_timeRef.try_as<wux::FrameworkElement>())
                sum += t.DesiredSize().Width + 10;
        double overflow = sum - hp.ActualWidth();
        if (overflow > 2) {
            for (int i = 0; i < 4; i++) {         // 先隐藏低优先级（天气最先）
                if (!g_seg[i] || g_segHidden[i]) continue;
                if (auto f = g_seg[i].try_as<wux::FrameworkElement>()) {
                    f.Visibility(wux::Visibility::Collapsed);
                    g_segHidden[i] = true;
                    log_line("PANEL reflow: hide %s (overflow %.0f)", SEGNAME[i], overflow);
                    return; // 一次一步，等下一轮布局
                }
            }
        } else if (overflow < -24) {
            double surplus = -overflow;
            for (int i = 3; i >= 0; i--) {        // 先恢复高优先级（农历最先）
                if (!g_seg[i] || !g_segHidden[i]) continue;
                if (g_segDesired[i] > 0 && g_segDesired[i] > surplus - 8) continue; // 放不下
                if (auto f = g_seg[i].try_as<wux::FrameworkElement>()) {
                    f.Visibility(wux::Visibility::Visible);
                    g_segHidden[i] = false;
                    log_line("PANEL reflow: show %s (surplus %.0f, need %.0f)", SEGNAME[i], surplus, g_segDesired[i]);
                    return;
                }
            }
        }
    } catch (...) { log_line("PANEL reflow EXCEPTION — lie flat"); }
}

// 面板构建/更新（UI 线程，经 dispatched lambda 调用）。v34 reparent 结构：
// 原生 sp[Vertical, 不碰] = [自建横板[天气|节日|节气|农历|Time], Date(隐藏)]
static HRESULT PanelBuild(bool rebuildAfterGen) {
    unsigned long long hTime = 0, hStack = 0, hCont = 0, hDTIC = 0; unsigned gen = 0;
    if (!RequireHandles(&hTime, &hStack, &hCont, &hDTIC, &gen)) return E_NOT_VALID_STATE;
    if (!g_diag) return E_NOT_VALID_STATE;
    auto getObj = [](unsigned long long h) -> wfnd::IInspectable {
        wfnd::IInspectable o{ nullptr };
        if (h && g_diag) {
            HRESULT hr = g_diag->GetIInspectableFromHandle((InstanceHandle)h,
                reinterpret_cast<::IInspectable**>(winrt::put_abi(o)));
            if (FAILED(hr)) o = nullptr;
        }
        return o;
    };
    auto sp = getObj(hStack).try_as<wux::Controls::StackPanel>();
    auto tb = getObj(hTime).try_as<wux::Controls::TextBlock>();
    if (!sp || !tb) return E_NOINTERFACE;

    if (!g_hasData) { // 无 host 数据时给一组默认假数据（C1 静态定义）
        wcsncpy_s(g_segText[0], L"晴 26°C", _TRUNCATE);
        wcsncpy_s(g_segText[1], L"教师节", _TRUNCATE);
        wcsncpy_s(g_segText[2], L"白露", _TRUNCATE);
        wcsncpy_s(g_segText[3], L"七月廿二", _TRUNCATE);
        g_hasData = true;
    }

    // Date 元素（按名在 sp 内找）+ Time 原位记录
    wfnd::IInspectable dateIns{ nullptr };
    int timeIdx = -1;
    auto ch0 = sp.Children();
    for (uint32_t c = 0; c < ch0.Size(); c++) {
        if (auto f = ch0.GetAt(c).try_as<wux::FrameworkElement>()) {
            if (f.Name() == L"DateInnerTextBlock") dateIns = f;
            if (winrt::get_abi(ch0.GetAt(c)) == winrt::get_abi(tb.as<wux::UIElement>())) timeIdx = (int)c;
        }
    }

    // 快照 + 引用（原生布局属性一律不写——v33 拉锯教训）
    g_snapDateVisibility = -1;
    if (dateIns) {
        g_dateRef = dateIns;
        if (auto d = dateIns.try_as<wux::FrameworkElement>())
            g_snapDateVisibility = (int)d.Visibility();
    }
    g_snapTimeIndex = timeIdx;
    g_spRef = sp; g_timeRef = tb;
    g_contRef = getObj(hCont);

    // Date 隐藏（一次性；v33 实证系统不复活）
    if (auto d = g_dateRef.try_as<wux::FrameworkElement>()) d.Visibility(wux::Visibility::Collapsed);

    // 自建横板（新建或复用）
    wux::Controls::StackPanel hp{ nullptr };
    bool makeHpanel = !g_hpanelRef;
    if (makeHpanel) {
        hp = wux::Controls::StackPanel{};
        hp.Orientation(wux::Controls::Orientation::Horizontal);
        hp.VerticalAlignment(wux::VerticalAlignment::Center);
        hp.MaxWidth(g_style.capw);
        g_hpanelRef = hp;
    } else {
        hp = g_hpanelRef.try_as<wux::Controls::StackPanel>();
        hp.MaxWidth(g_style.capw);
    }

    // 4 段（新建或更新文本/样式）
    for (int i = 0; i < 4; i++) {
        wux::Controls::TextBlock seg{ nullptr };
        if (g_seg[i]) seg = g_seg[i].try_as<wux::Controls::TextBlock>();
        bool makeNew = !seg;
        if (makeNew) seg = wux::Controls::TextBlock{};
        seg.Text(winrt::hstring(g_segText[i]));
        try { seg.FontSize(tb.FontSize() * g_style.fontscale); } catch (...) {}
        try { seg.FontFamily(tb.FontFamily()); } catch (...) {}
        try { seg.FontWeight(tb.FontWeight()); } catch (...) {}
        try { seg.Foreground(tb.Foreground()); } catch (...) {}
        seg.VerticalAlignment(wux::VerticalAlignment::Center);
        seg.TextWrapping(wux::TextWrapping::NoWrap);
        seg.TextTrimming(wux::TextTrimming::CharacterEllipsis);
        seg.MaxWidth(g_style.segmaxw);
        seg.Margin({ i == 0 ? 0.0 : 10.0, 0, 0, 0 });
        if (makeNew) {
            g_seg[i] = seg;
            g_segHidden[i] = false;
            hp.Children().Append(g_seg[i].try_as<wux::UIElement>());
        }
    }
    // Time reparent 到横板末位（系统 VM 每秒仍按引用写 Text，与父无关）
    auto tbU = tb.as<wux::UIElement>();
    {
        auto curParent = wuxm::VisualTreeHelper::GetParent(tb);
        if (curParent) {
            if (auto pp = curParent.try_as<wux::Controls::Panel>()) {
                auto pch = pp.Children();
                for (uint32_t c = 0; c < pch.Size(); c++)
                    if (winrt::get_abi(pch.GetAt(c)) == winrt::get_abi(tbU)) {
                        InterlockedExchange(&g_selfReparent, 1); // 自我 REM 抑制窗口开
                        pch.RemoveAt(c);
                        InterlockedExchange(&g_selfReparent, 0); // 同步回调已在窗口内消化
                        break;
                    }
            }
        }
        hp.Children().Append(tbU);
    }
    // 横板插到原生面板第 0 位
    {
        auto spCh = sp.Children();
        auto hpU = g_hpanelRef.try_as<wux::UIElement>();
        bool present = false;
        for (uint32_t c = 0; c < spCh.Size(); c++)
            if (winrt::get_abi(spCh.GetAt(c)) == winrt::get_abi(hpU)) { present = true; break; }
        if (!present) spCh.InsertAt(0, hpU);
    }
    g_panelGen = gen;
    InterlockedExchange(&g_panelOn, 1);
    {
        char s0[96], s1[96], s2[96], s3[96];
        w2a(g_segText[0], s0, sizeof(s0)); w2a(g_segText[1], s1, sizeof(s1));
        w2a(g_segText[2], s2, sizeof(s2)); w2a(g_segText[3], s3, sizeof(s3));
        log_line("PANEL %s gen=%u segs=[%s|%s|%s|%s|TIME] scale=%.2f capw=%.0f (timeIdx=%d, native sp untouched)",
                 rebuildAfterGen ? "rebuild" : "build", gen, s0, s1, s2, s3,
                 g_style.fontscale, g_style.capw, timeIdx);
    }

    // C2 输入拦截 Border（盖住时钟内容区，截获指针输入）
    if (g_style.input && !g_borderRef) {
        try {
            auto contGrid = g_contRef.try_as<wux::Controls::Grid>();
            if (contGrid) {
                wux::Controls::Border border;
                wuxm::SolidColorBrush brush;
                brush.Color(winrt::Windows::UI::Colors::Transparent());
                border.Background(brush);
                g_borderRef = border;
                winrt::weak_ref<wux::Controls::Border> bw = border;
                // v38：指针按下/释放一律标记 Handled——阻断向原生按钮的冒泡
                // （v37 实测：时钟的「通知设置」飞出在右键【按下】即经冒泡触发，
                // 仅处理 RightTapped 手势拦不住它；Tapped/RightTapped 是独立手势
                // 事件，不受 Pointer 事件 Handled 影响，实测验证）
                // v39：移动/进入也标记 Handled——v38 实测悬停 tooltip
                // （「2026/9/13 周日 … (当地时间)」）经移动事件冒泡给原生按钮触发；
                // 现状（旧覆盖层）无 tooltip，等价性要求一并抑制。
                border.PointerPressed([](wfnd::IInspectable const&, wux::Input::PointerRoutedEventArgs const& e) {
                    try { e.Handled(true); } catch (...) {}
                });
                border.PointerReleased([](wfnd::IInspectable const&, wux::Input::PointerRoutedEventArgs const& e) {
                    try { e.Handled(true); } catch (...) {}
                });
                border.PointerMoved([](wfnd::IInspectable const&, wux::Input::PointerRoutedEventArgs const& e) {
                    try { e.Handled(true); } catch (...) {}
                });
                border.PointerEntered([](wfnd::IInspectable const&, wux::Input::PointerRoutedEventArgs const& e) {
                    try { e.Handled(true); } catch (...) {}
                });
                border.PointerExited([](wfnd::IInspectable const&, wux::Input::PointerRoutedEventArgs const& e) {
                    try { e.Handled(true); } catch (...) {}
                });
                // v40：ContextRequested 是 Win11 右键菜单的统一触发事件（右压即发、
                // 经冒泡到原生按钮开「通知设置」飞出）；v38 的按下手势拦截被实测为
                // 不稳定（v39 复现），在正主事件上标记 Handled 才是确定性抑制。
                border.ContextRequested([](wfnd::IInspectable const&, wux::Input::ContextRequestedEventArgs const& e) {
                    try { e.Handled(true); } catch (...) {}
                });
                unsigned myGen = gen;
                border.Tapped([myGen, bw](wfnd::IInspectable const&, wux::Input::TappedRoutedEventArgs const& e) {
                    try {
                        log_line("TAP left fired");
                        e.Handled(true);
                        double x = -1, y = -1;
                        try { if (auto b = bw.get()) { auto p = e.GetPosition(b); x = p.X; y = p.Y; } } catch (...) {}
                        SendTapEvent("left", x, y);
                    } catch (...) { log_line("TAP left handler EXCEPTION"); }
                });
                border.RightTapped([myGen, bw](wfnd::IInspectable const&, wux::Input::RightTappedRoutedEventArgs const& e) {
                    try {
                        log_line("TAP right fired");
                        e.Handled(true);
                        double x = -1, y = -1;
                        try { if (auto b = bw.get()) { auto p = e.GetPosition(b); x = p.X; y = p.Y; } } catch (...) {}
                        SendTapEvent("right", x, y);
                    } catch (...) { log_line("TAP right handler EXCEPTION"); }
                });
                contGrid.Children().Append(g_borderRef.try_as<wux::UIElement>());
                log_line("PANEL input border appended to ContainerGrid (children=%u)", contGrid.Children().Size());
            }
        } catch (const winrt::hresult_error& e) {
            log_line("PANEL border hresult hr=0x%08lx", (unsigned long)e.code().value);
        } catch (...) { log_line("PANEL border EXCEPTION"); }
    }

    // 事件订阅（每次 build 重挂——元素随 gen 换新，旧 token 随旧元素消亡）
    try {
        if (g_eventsOn) { try { sp.SizeChanged(g_szToken); sp.ActualThemeChanged(g_themeToken); } catch (...) {} g_eventsOn = false; }
        // v41：SizeChanged 挂在自建横板上并量自建横板——v40 实测挂在原生（垂直）面板上
        // desired≈actual 永远测不到溢出，capw 过小时时间被裁（违反「时间段永不丢」）
        g_szToken = g_hpanelRef.as<wux::FrameworkElement>().SizeChanged([](wfnd::IInspectable const& s, wux::SizeChangedEventArgs const&) {
            if (auto hpt = s.try_as<wux::Controls::StackPanel>()) PanelReflow(hpt);
        });
        g_themeToken = sp.ActualThemeChanged([](wux::FrameworkElement const&, wfnd::IInspectable const&) {
            try { // 主题翻转：段前景色跟随 Time 重新取
                if (auto t = g_timeRef.try_as<wux::Controls::TextBlock>()) {
                    auto fg = t.Foreground();
                    for (int i = 0; i < 4; i++)
                        if (auto s = g_seg[i].try_as<wux::Controls::TextBlock>()) s.Foreground(fg);
                    log_line("PANEL theme changed: foreground re-copied");
                }
            } catch (...) { log_line("PANEL theme EXCEPTION"); }
        });
        g_eventsOn = true;
    } catch (...) { log_line("PANEL events attach failed — lie flat"); }

    // 维护定时器（会话级，唯一；Stop 后可重启）
    try {
        if (!g_timer) {
            g_timer = wux::DispatcherTimer{};
            g_timer.Interval(std::chrono::milliseconds{ 1000 });
            g_timer.Tick([](wfnd::IInspectable const&, wfnd::IInspectable const&) { PanelTick(); });
            log_line("PANEL maintenance timer created (1s)");
        }
        g_timer.Start();
    } catch (...) { log_line("PANEL timer start failed — lie flat"); }
    return S_OK;
}

// 面板移除+原生恢复（UI 线程）。restoreNative=false 仅摘除我们自己的元素。
static void PanelFree(bool restoreNative) {
    try { if (g_timer) { g_timer.Stop(); } } catch (...) {}
    g_timer = nullptr;
    if (g_eventsOn) {
        if (auto sp = g_spRef.try_as<wux::Controls::StackPanel>())
            try { sp.ActualThemeChanged(g_themeToken); } catch (...) {}
        if (auto hp = g_hpanelRef.try_as<wux::FrameworkElement>())
            try { hp.SizeChanged(g_szToken); } catch (...) {}
    }
    g_eventsOn = false;
    // 摘横板（连同段与 reparent 的 Time——先把 Time 放回原生面板原位）
    auto sp = g_spRef ? g_spRef.try_as<wux::Controls::StackPanel>() : nullptr;
    auto hp = g_hpanelRef ? g_hpanelRef.try_as<wux::Controls::StackPanel>() : nullptr;
    if (sp) {
        auto spCh = sp.Children();
        auto hpU = g_hpanelRef.try_as<wux::UIElement>();
        int hpIdx = -1;
        for (uint32_t c = 0; c < spCh.Size(); c++)
            if (hpU && winrt::get_abi(spCh.GetAt(c)) == winrt::get_abi(hpU)) { hpIdx = (int)c; break; }
        if (hpIdx >= 0) {
            // Time 先回原位（相对 Date 的原始次序：原 idx 记录于 g_snapTimeIndex）
            if (hp && g_timeRef) {
                auto tbU = g_timeRef.try_as<wux::UIElement>();
                auto hpCh = hp.Children();
                for (uint32_t c = 0; c < hpCh.Size(); c++)
                    if (tbU && winrt::get_abi(hpCh.GetAt(c)) == winrt::get_abi(tbU)) { hpCh.RemoveAt(c); break; }
            }
            spCh.RemoveAt((uint32_t)hpIdx);
            if (restoreNative && g_timeRef && g_snapTimeIndex >= 0) {
                int at = g_snapTimeIndex;
                if (at > (int)spCh.Size()) at = (int)spCh.Size();
                spCh.InsertAt((uint32_t)at, g_timeRef.try_as<wux::UIElement>());
            }
        }
    }
    for (int i = 0; i < 4; i++) { g_seg[i] = nullptr; g_segHidden[i] = false; g_segDesired[i] = 0; }
    g_hpanelRef = nullptr;
    // 摘输入 Border
    if (g_borderRef) {
        try {
            if (auto cg = g_contRef.try_as<wux::Controls::Grid>()) {
                auto ch = cg.Children();
                auto ui = g_borderRef.try_as<wux::UIElement>();
                for (uint32_t c = 0; c < ch.Size(); c++) {
                    if (ui && winrt::get_abi(ch.GetAt(c)) == winrt::get_abi(ui)) { ch.RemoveAt(c); break; }
                }
            }
        } catch (...) {}
        g_borderRef = nullptr;
    }
    // Date 恢复（D7：只恢复仍属于自己的改动）
    if (restoreNative) {
        if (auto d = g_dateRef.try_as<wux::FrameworkElement>(); d && g_snapDateVisibility >= 0)
            try { d.Visibility((wux::Visibility)g_snapDateVisibility); } catch (...) {}
    }
    g_snapDateVisibility = -1; g_snapTimeIndex = -1;
    g_spRef = nullptr; g_timeRef = nullptr; g_dateRef = nullptr; g_contRef = nullptr;
    InterlockedExchange(&g_panelOn, 0);
    log_line("PANEL free (restoreNative=%d)", (int)restoreNative);
}

// mode: 10=c1set(build/update) 11=c1free
static int RunC1Job(std::shared_ptr<UiJob> job, DWORD timeoutMs) {
    wuc::CoreDispatcher disp{ nullptr };
    AcquireSRWLockShared(&g_stateLock); disp = g_disp; ReleaseSRWLockShared(&g_stateLock);
    if (!disp) { log_line("C1 no dispatcher (mode=%d)", job->mode); job->done = 3; return 2; }
    try {
        disp.RunAsync(wuc::CoreDispatcherPriority::Normal, [job]() {
            try {
                if (job->mode == 10) {
                    job->hr = PanelBuild(false);
                    InterlockedExchange(&job->done, SUCCEEDED(job->hr) ? 1 : 2);
                } else if (job->mode == 11) {
                    PanelFree(true);
                    job->hr = S_OK;
                    InterlockedExchange(&job->done, 1);
                } else { job->hr = E_INVALIDARG; InterlockedExchange(&job->done, 2); }
            } catch (...) { job->hr = E_FAIL; InterlockedExchange(&job->done, 2); }
        });
        for (DWORD t = 0; t < timeoutMs; t += 25) { if (job->done) break; Sleep(25); }
        if (!job->done) { log_line("C1 timeout mode=%d", job->mode); return 1; }
        return job->done == 1 ? 0 : 2;
    } catch (...) { job->done = 2; return 2; }
}

// B0：pipe 真断开 → 自动恢复 + 停活动（不依赖宿主）
static void AutoRestoreOnDisconnect() {
    log_line("AUTO pipe disconnected -> restore sequence");
    // C1：断线=会话结束，面板整体摘除+原生恢复（UI 线程），并清 wanted（重连需 host 重新下发）
    InterlockedExchange(&g_panelWanted, 0);
    {
        wuc::CoreDispatcher disp{ nullptr };
        AcquireSRWLockShared(&g_stateLock); disp = g_disp; ReleaseSRWLockShared(&g_stateLock);
        if (disp && g_panelOn) {
            disp.RunAsync(wuc::CoreDispatcherPriority::Normal, []() {
                try { PanelFree(true); log_line("AUTO panel freed on disconnect"); }
                catch (...) { log_line("AUTO PanelFree EXCEPTION"); }
            });
        }
    }
    CmdRestore("auto");
    TapObject* o = cur_obj();
    if (o) o->UnadvisePublic();
    else log_line("AUTO no object to unadvise");
}

// 进程内初始化（TranslucentTB 生产通道实证配方；A 阶段血泪全沿用）：
// 每次尝试用【新线程】+【递增端点名 VisualDiagConnection{N}】；pid=GetCurrentProcessId()；
// wszDllXamlDiagnostics=nullptr；TAP dll=自身路径；失败重试 ≤60 次 × 500ms。
struct InitArgs {
    HRESULT (WINAPI* ixde)(LPCWSTR, DWORD, LPCWSTR, LPCWSTR, REFCLSID, LPCWSTR);
    DWORD pid; LPCWSTR loc; LPCWSTR conn; const CLSID* clsid; HRESULT hr;
};
static DWORD WINAPI InitAttemptThread(LPVOID p) {
    InitArgs* a = (InitArgs*)p;
    a->hr = a->ixde(a->conn, a->pid, nullptr, a->loc, *a->clsid, nullptr);
    return 0;
}
static HRESULT RunInitAttempt(int attempt) {
    HMODULE mod = nullptr;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                            (LPCWSTR)&RunInitAttempt, &mod)) return E_FAIL;
    WCHAR loc[MAX_PATH];
    if (!mod || !GetModuleFileNameW(mod, loc, MAX_PATH)) return E_FAIL;
    HMODULE wux = LoadLibraryExW(L"Windows.UI.Xaml.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!wux) return HRESULT_FROM_WIN32(GetLastError());
    auto ixde = (HRESULT (WINAPI*)(LPCWSTR, DWORD, LPCWSTR, LPCWSTR, REFCLSID, LPCWSTR))
        GetProcAddress(wux, "InitializeXamlDiagnosticsEx");
    if (!ixde) return E_FAIL;
    WCHAR conn[64];
    _snwprintf_s(conn, 64, _TRUNCATE, L"VisualDiagConnection%d", attempt);
    static const CLSID kClsid = CLSID_LicalClockTap;
    InitArgs a{ ixde, GetCurrentProcessId(), loc, conn, &kClsid, E_FAIL };
    HANDLE t = CreateThread(NULL, 0, InitAttemptThread, &a, 0, NULL);
    if (!t) return E_FAIL;
    WaitForSingleObject(t, INFINITE);
    CloseHandle(t);
    return a.hr;
}

static DWORD WINAPI InstallThread(LPVOID) {
    for (int attempt = 1; attempt <= 60; attempt++) {
        HRESULT hr = RunInitAttempt(attempt);
        log_line("Install attempt=%d hr=0x%08lx", attempt, (unsigned long)hr);
        if (SUCCEEDED(hr)) return 0;
        Sleep(500);
    }
    log_line("Install: gave up after 60 attempts");
    return 1;
}

// 兼容旧 selfinit 命令
static HRESULT SelfInit() {
    HRESULT hr = RunInitAttempt(1000);
    log_line("SelfInit hr=0x%08lx", (unsigned long)hr);
    return hr;
}

// ── pipe 客户端线程：host 控制命令（有界，异常即断开重连）──
static void send_loaded(HANDLE h) {
    char out[192];
    _snprintf_s(out, sizeof(out), _TRUNCATE,
        "{\"v\":2,\"t\":\"loaded\",\"ver\":%d,\"advises\":%ld}\n", TAPVER, ((long)g_adviseCount));
    DWORD w = 0;
    AcquireSRWLockExclusive(&g_sendLock);
    WriteFile(h, out, (DWORD)strlen(out), &w, NULL);
    ReleaseSRWLockExclusive(&g_sendLock);
}
static DWORD WINAPI pipe_thread(LPVOID) {
    log_line("pipe: client thread start");
    for (int retry = 0;;) {
        HANDLE h = CreateFileW(g_pipeName, GENERIC_READ | GENERIC_WRITE, 0, NULL,
                               OPEN_EXISTING, 0, NULL);
        if (h == INVALID_HANDLE_VALUE) {
            if (retry < 3 || retry % 600 == 0)
                log_line("pipe: CreateFile failed gle=%lu retry=%d", GetLastError(), retry);
            if (++retry > 60000) retry = 60000;
            Sleep(100); continue;
        }
        retry = 0;
        InterlockedExchangePointer(&g_pipeH, h);
        log_line("pipe: connected");
        send_loaded(h);
        char out[512];
        char buf[1024];
        wchar_t wcmd[128];
        for (;;) {
            // v37：Peek 轮询读（血泪 #6 的另一半——v30 只改了 host 侧；C36 实测
            // 阻塞 ReadFile 会把整个管道实例的写 I/O 串行化：tapq 线程的 WriteFile
            // 直到 pipe 线程被下一条命令唤醒才完成，tap 事件时延被拉到秒级）
            DWORD avail = 0;
            if (!PeekNamedPipe(h, NULL, 0, NULL, &avail, NULL)) {
                log_line("pipe: peek failed gle=%lu", GetLastError());
                break;
            }
            if (avail == 0) { Sleep(20); continue; }
            DWORD n = 0;
            BOOL ok = ReadFile(h, buf, sizeof(buf) - 1, &n, NULL);
            if (!ok) { log_line("pipe: ReadFile failed gle=%lu", GetLastError()); break; }
            if (n == 0) continue; // 虚唤醒防御保留
            buf[n] = 0;
            char* nl = strchr(buf, '\n'); if (nl) *nl = 0;
            char* cr = strchr(buf, '\r'); if (cr) *cr = 0;
            char cmd[32] = ""; char arg[128] = "";
            sscanf_s(buf, "%31s %127s", cmd, (unsigned)sizeof(cmd), arg, (unsigned)sizeof(arg));
            if (!cmd[0]) continue;
            const char* rest = buf + strlen(cmd); // c1set 用整行参数（有界）
            while (*rest == ' ') rest++;
            TouchRecv(); // 心跳基准：任何命令都刷新
            log_line("CMD '%s' arg='%.64s' restlen=%d", cmd, arg, (int)strlen(rest));
            try {
                if (!strcmp(cmd, "unadvise")) {
                    TapObject* o = cur_obj();
                    if (o) o->UnadvisePublic(); else log_line("unadvise: no object");
                    _snprintf_s(out, sizeof(out), _TRUNCATE,
                        "{\"v\":2,\"t\":\"unadvised\",\"advises\":%ld,\"unadvises\":%ld}\n",
                        ((long)g_adviseCount), ((long)g_unadviseCount));
                    send_line(out);
                } else if (!strcmp(cmd, "advise")) {
                    TapObject* o = cur_obj();
                    if (o) o->AdvisePublic(); else log_line("advise: no object");
                    Sleep(200); // 等 Advise 线程落位（同步重放完成）后回报
                    _snprintf_s(out, sizeof(out), _TRUNCATE,
                        "{\"v\":2,\"t\":\"advised\",\"advises\":%ld,\"unadvises\":%ld}\n",
                        ((long)g_adviseCount), ((long)g_unadviseCount));
                    send_line(out);
                    // 重发 ready（幂等）：再次 advise 时元素未变则重放不触发 newlyLocated，
                    // 但 host 侧 ready=「面板可用」语义需要每个会话都拿到
                    {
                        bool loc = false; unsigned gen = 0; unsigned long long hT = 0;
                        AcquireSRWLockShared(&g_stateLock);
                        loc = g_located; gen = g_gen; hT = g_hTime;
                        ReleaseSRWLockShared(&g_stateLock);
                        if (loc) {
                            _snprintf_s(out, sizeof(out), _TRUNCATE,
                                "{\"v\":2,\"t\":\"ready\",\"gen\":%u,\"hTime\":\"%llX\"}\n", gen, hT);
                            send_line(out);
                        }
                    }
                } else if (!strcmp(cmd, "stats")) {
                    unsigned gen = 0; bool loc = false; bool mod = false; bool disp = false;
                    AcquireSRWLockShared(&g_stateLock);
                    gen = g_gen; loc = g_located; mod = g_snap.modified;
                    ReleaseSRWLockShared(&g_stateLock);
                    disp = g_disp ? true : false;
                    _snprintf_s(out, sizeof(out), _TRUNCATE,
                        "{\"v\":2,\"t\":\"stats\",\"lines\":%ld,\"objs\":%ld,\"advises\":%ld,\"unadvises\":%ld,\"gen\":%u,\"located\":%d,\"modified\":%d,\"disp\":%d,\"probe\":%ld,\"panel\":%ld,\"panelGen\":%u}\n",
                        ((long)g_logLines), ((long)g_objectsAlive), ((long)g_adviseCount),
                        ((long)g_unadviseCount), gen, (int)loc, (int)mod, (int)disp, g_probeOn,
                        g_panelOn, g_panelGen);
                    send_line(out);
                } else if (!strcmp(cmd, "selfinit")) {
                    HRESULT hr = SelfInit();
                    char out2[96];
                    _snprintf_s(out2, sizeof(out2), _TRUNCATE,
                        "{\"v\":2,\"t\":\"selfinit\",\"hr\":%lu}\n", (unsigned long)hr);
                    send_line(out2);
                } else if (!strcmp(cmd, "ping")) {
                    send_line("{\"v\":2,\"t\":\"pong\"}\n");
                } else if (!strcmp(cmd, "hitclock")) {
                    // 判别：屏幕时钟矩形 HitTest 出的可见元素句柄 vs 我们跟踪的 g_hTime
                    // （引擎调用，按血泪 #5 纪律放 dispatched lambda）
                    wuc::CoreDispatcher disp{ nullptr };
                    AcquireSRWLockShared(&g_stateLock); disp = g_disp; ReleaseSRWLockShared(&g_stateLock);
                    if (!disp) { send_line("{\"v\":2,\"t\":\"hittest\",\"err\":1}\n"); }
                    else {
                        auto job = std::make_shared<UiJob>();
                        try {
                            disp.RunAsync(wuc::CoreDispatcherPriority::Normal, [job]() {
                                RECT rc = { 3640, 2076, 3819, 2160 };
                                unsigned cnt = 0;
                                InstanceHandle* handles = nullptr;
                                HRESULT hr = g_diag ? g_diag->HitTest(rc, &cnt, &handles) : E_FAIL;
                                log_line("HITTEST hr=0x%08lx cnt=%u trackedTime=%llX",
                                         (unsigned long)hr, cnt, g_hTime);
                                if (SUCCEEDED(hr)) {
                                    for (unsigned i = 0; i < cnt && handles; i++) {
                                        log_line("HITTEST[%u] h=%llX %s", i,
                                                 (unsigned long long)handles[i],
                                                 (unsigned long long)handles[i] == g_hTime
                                                     ? "<== TRACKED TIME" : "");
                                    }
                                    if (handles) CoTaskMemFree(handles);
                                }
                            });
                            for (DWORD t = 0; t < 3000; t += 25) { if (job->done) break; Sleep(25); }
                        } catch (...) { log_line("HITTEST RunAsync EXCEPTION"); }
                        send_line("{\"v\":2,\"t\":\"hittest\",\"done\":1}\n");
                    }
                } else if (!strcmp(cmd, "props")) {
                    CmdProps();
                } else if (!strcmp(cmd, "c0tree")) {
                    CmdC0(3, "c0tree");
                } else if (!strcmp(cmd, "c0ins")) {
                    CmdC0(4, "c0ins");
                } else if (!strcmp(cmd, "c0meas")) {
                    CmdC0(5, "c0meas");
                } else if (!strcmp(cmd, "c0rm")) {
                    CmdC0(6, "c0rm");
                } else if (!strcmp(cmd, "c0add")) {
                    CmdC0(7, "c0add");
                } else if (!strcmp(cmd, "c1set")) {
                    // host 下发五段数据+样式（协议 §4.3 骨架的 C 阶段形态）；限界解析
                    if (strlen(rest) < 2 || strlen(rest) > 700) {
                        send_line("{\"v\":2,\"t\":\"c1set\",\"err\":\"bad_len\"}\n");
                    } else {
                        PanelStyle st = g_style;
                        bool any = false; char err[64] = "";
                        if (JGetDbl(rest, "fontscale", &st.fontscale)) any = true;
                        if (JGetDbl(rest, "segmaxw", &st.segmaxw)) any = true;
                        if (JGetDbl(rest, "capw", &st.capw)) any = true;
                        double dInput = -1;
                        if (JGetDbl(rest, "input", &dInput)) { st.input = dInput > 0.5 ? 1 : 0; any = true; }
                        wchar_t tmp[64];
                        struct { const char* key; int idx; } segs[4] = {
                            {"weather",0},{"festival",1},{"term",2},{"lunar",3} };
                        for (int i = 0; i < 4; i++) {
                            if (JGetStr(rest, segs[i].key, tmp, 64)) {
                                wcsncpy_s(g_segText[segs[i].idx], tmp, _TRUNCATE);
                                any = true;
                            }
                        }
                        if (!any) {
                            _snprintf_s(err, sizeof(err), _TRUNCATE, "no_fields");
                            send_line("{\"v\":2,\"t\":\"c1set\",\"err\":\"no_fields\"}\n");
                        } else {
                            g_style = st;
                            g_hasData = true;
                            InterlockedExchange(&g_panelWanted, 1);
                            auto job = std::make_shared<UiJob>(); job->mode = 10;
                            int rc = RunC1Job(job, 8000);
                            char a2[160];
                            _snprintf_s(a2, sizeof(a2), _TRUNCATE,
                                "{\"v\":2,\"t\":\"c1set\",\"rc\":%d,\"hr\":%lu}\n",
                                rc, (unsigned long)job->hr);
                            send_line(a2);
                        }
                    }
                } else if (!strcmp(cmd, "c1free")) {
                    InterlockedExchange(&g_panelWanted, 0);
                    auto job = std::make_shared<UiJob>(); job->mode = 11;
                    int rc = RunC1Job(job, 8000);
                    char a2[128];
                    _snprintf_s(a2, sizeof(a2), _TRUNCATE,
                        "{\"v\":2,\"t\":\"c1free\",\"rc\":%d,\"hr\":%lu}\n",
                        rc, (unsigned long)job->hr);
                    send_line(a2);
                } else if (!strcmp(cmd, "settext") || !strcmp(cmd, "settext2")) {
                    int route = !strcmp(cmd, "settext") ? 1 : 2;
                    MultiByteToWideChar(CP_UTF8, 0, arg, -1, wcmd, 128);
                    CmdSet(route, wcmd);
                } else if (!strcmp(cmd, "restore")) {
                    CmdRestore("host");
                }
            } catch (...) {
                log_line("pipe: command '%s' EXCEPTION — dropped", cmd);
            }
        }
        AcquireSRWLockExclusive(&g_sendLock);
        InterlockedExchangePointer(&g_pipeH, INVALID_HANDLE_VALUE);
        ReleaseSRWLockExclusive(&g_sendLock);
        CloseHandle(h);
        // B0：真断开（宿主退出/被杀）→ 自动恢复全部被改属性 + Unadvise，不等指令
        try { AutoRestoreOnDisconnect(); } catch (...) { log_line("AUTO EXCEPTION"); }
        log_line("pipe: disconnected, will reconnect");
        Sleep(200);
    }
}

// ── ClassFactory ─────────────────────────────────────
class Factory : public IClassFactory {
    LONG m_ref = 1;
public:
    STDMETHODIMP QueryInterface(REFIID riid, void** ppv) {
        if (!ppv) return E_POINTER;
        if (IsEqualIID(riid, IID_IUnknown) || IsEqualIID(riid, IID_IClassFactory)) {
            *ppv = static_cast<IClassFactory*>(this); AddRef(); return S_OK;
        }
        *ppv = nullptr; return E_NOINTERFACE;
    }
    STDMETHODIMP_(ULONG) AddRef()  { return InterlockedIncrement(&m_ref); }
    STDMETHODIMP_(ULONG) Release() { LONG r = InterlockedDecrement(&m_ref); if (!r) delete this; return r; }
    STDMETHODIMP CreateInstance(IUnknown* pOuter, REFIID riid, void** ppv) {
        if (pOuter) return CLASS_E_NOAGGREGATION;
        ensure_flush_thread();
        TapObject* o = new (std::nothrow) TapObject();
        if (!o) return E_OUTOFMEMORY;
        register_obj(o);
        o->AddRef();
        HRESULT hr = o->QueryInterface(riid, ppv);
        o->Release();
        if (SUCCEEDED(hr)) {
            static LONG pipeStarted = 0;
            if (!InterlockedExchange(&pipeStarted, 1))
                CreateThread(NULL, 0, pipe_thread, NULL, 0, NULL);
        }
        return hr;
    }
    STDMETHODIMP LockServer(BOOL) { return S_OK; }
};

extern "C" STDAPI DllGetClassObject(REFCLSID rclsid, REFIID riid, void** ppv) {
    try {
        ensure_flush_thread();
        log_line("DllGetClassObject rclsid={%08lX-%04X-%04X} riid={%08lX-%04X-%04X}",
                 (unsigned long)rclsid.Data1, rclsid.Data2, rclsid.Data3,
                 (unsigned long)riid.Data1, riid.Data2, riid.Data3);
        if (!IsEqualCLSID(rclsid, CLSID_LicalClockTap)) { log_line("DllGetClassObject: clsid mismatch"); return CLASS_E_CLASSNOTAVAILABLE; }
        static Factory* f = nullptr;
        if (!f) f = new Factory();
        HRESULT hr = f->QueryInterface(riid, ppv);
        log_line("DllGetClassObject factory QI hr=0x%08lx", (unsigned long)hr);
        return hr;
    } catch (...) { return E_FAIL; }
}
extern "C" STDAPI DllCanUnloadNow() { return S_FALSE; }

BOOL APIENTRY DllMain(HMODULE, DWORD reason, LPVOID) {
    // 钩子注入通道（TranslucentTB 同款）：宿主经 SetWindowsHookEx 把本 DLL 载入 explorer；
    // DllMain 里仅 CreateThread（允许），初始化全部在 InstallThread。仅当宿主是 explorer.exe。
    if (reason == DLL_PROCESS_ATTACH) {
        init_paths();
        WCHAR exe[MAX_PATH]; DWORD n = GetModuleFileNameW(NULL, exe, MAX_PATH);
        if (n > 0) {
            const WCHAR* base = wcsrchr(exe, L'\\');
            base = base ? base + 1 : exe;
            if (_wcsicmp(base, L"explorer.exe") == 0) {
                // 【血泪 #1】自钉：初始化失败路径的卸载守卫（详见文件头说明）
                HMODULE self = nullptr;
                if (GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                                       GET_MODULE_HANDLE_EX_FLAG_PIN,
                                       (LPCWSTR)&DllMain, &self)) {
                    log_line("DllMain: pinned self in explorer (failure-path unload guard)");
                }
                ensure_flush_thread();
                log_line("DllMain: loaded into explorer, starting InstallThread");
                HANDLE t = CreateThread(NULL, 0, InstallThread, NULL, 0, NULL);
                if (t) CloseHandle(t);
            }
        }
    }
    /* 不调用 DisableThreadLibraryCalls：/MT 静态 CRT 依赖线程通知 */
    return TRUE;
}

// 宿主 SetWindowsHookEx 用的占位钩子过程（TranslucentTB CallWndProc 同款）
extern "C" __declspec(dllexport) LRESULT CALLBACK LicalHookProc(int nCode, WPARAM wParam, LPARAM lParam) {
    return CallNextHookEx(NULL, nCode, wParam, lParam);
}
