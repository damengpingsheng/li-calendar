// lical_clock_tap — B 阶段可逆修改 TAP DLL（方案 v2 §5 B）
// 纪律（方案 §6）：DllMain 仅返回 TRUE；不调用 DisableThreadLibraryCalls（/MT 静态 CRT）；
// 全 COM 入口 try/catch(...) 包裹；任何异常/失败一律躺平记日志，不重试。
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

#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.UI.Core.h>
#include <winrt/Windows.UI.Xaml.h>
#include <winrt/Windows.UI.Xaml.Controls.h>
namespace wux = winrt::Windows::UI::Xaml;
namespace wuc = winrt::Windows::UI::Core;
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
    for (;;) {
        Sleep(300);
        if (!InterlockedExchange(&g_flushQueued, 0)) continue;
        std::string out;
        AcquireSRWLockExclusive(&g_logLock);
        out.swap(g_logBuf);
        ReleaseSRWLockExclusive(&g_logLock);
        if (out.empty()) continue;
        HANDLE h = CreateFileW(g_logPath, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE,
                               NULL, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
        if (h == INVALID_HANDLE_VALUE) continue;
        DWORD written = 0;
        WriteFile(h, out.data(), (DWORD)out.size(), &written, NULL);
        CloseHandle(h);
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

// B0：pipe 真断开 → 自动恢复 + 停活动（不依赖宿主）
static void AutoRestoreOnDisconnect() {
    log_line("AUTO pipe disconnected -> restore sequence");
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
        char buf[512];
        wchar_t wcmd[128];
        for (;;) {
            DWORD n = 0;
            BOOL ok = ReadFile(h, buf, sizeof(buf) - 1, &n, NULL);
            if (!ok) { log_line("pipe: ReadFile failed gle=%lu", GetLastError()); break; }
            if (n == 0) continue; // TRUE+0 = 虚唤醒（A 阶段实测），连接仍活
            buf[n] = 0;
            char* nl = strchr(buf, '\n'); if (nl) *nl = 0;
            char* cr = strchr(buf, '\r'); if (cr) *cr = 0;
            char cmd[32] = ""; char arg[128] = "";
            sscanf_s(buf, "%31s %127s", cmd, (unsigned)sizeof(cmd), arg, (unsigned)sizeof(arg));
            if (!cmd[0]) continue;
            log_line("CMD '%s' arg='%s'", cmd, arg);
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
                        "{\"v\":2,\"t\":\"stats\",\"lines\":%ld,\"objs\":%ld,\"advises\":%ld,\"unadvises\":%ld,\"gen\":%u,\"located\":%d,\"modified\":%d,\"disp\":%d}\n",
                        ((long)g_logLines), ((long)g_objectsAlive), ((long)g_adviseCount),
                        ((long)g_unadviseCount), gen, (int)loc, (int)mod, (int)disp);
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
