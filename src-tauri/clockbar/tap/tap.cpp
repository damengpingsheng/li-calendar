// lical_clock_tap — A 阶段只读 TAP DLL（方案 v2 §5 A）
// 纪律（方案 §6）：DllMain 仅返回 TRUE；不调用 DisableThreadLibraryCalls（/MT 静态 CRT）；
// 全 COM 入口 try/catch(...) 包裹；只做枚举-记录，绝不 SetProperty/AddChild；
// 异常/失败一律躺平记日志，不重试。
// build: build_tap.cmd（cl /LD /MT /EHsc）
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <objbase.h>
#include <ocidl.h>
#include <xamlom.h>
#include <stdio.h>
#include <string>

// ── 身份 ──────────────────────────────────────────────
// CLSID_LicalClockTap { D4C1B77E-4E2F-4E7A-9B31-5F0A6C2E8B14 }
static const CLSID CLSID_LicalClockTap = {
    0xd4c1b77e, 0x4e2f, 0x4e7a, {0x9b, 0x31, 0x5f, 0x0a, 0x6c, 0x2e, 0x8b, 0x14} };

static const IID IID_IVisualTreeService_ = {
    0xa593b11a, 0xd17f, 0x48bb, {0x8f, 0x66, 0x83, 0x91, 0x07, 0x31, 0xc8, 0xa5} };
// IVisualTreeService3 {0E79C6E0-85A0-4BE8-B41A-655CF1FD19BD}（ExplorerTAP 用它 Advise 成功并看到 TaskbarFrame）
static const IID IID_IVisualTreeService3_ = {
    0x0e79c6e0, 0x85a0, 0x4be8, {0xb4, 0x1a, 0x65, 0x5c, 0xf1, 0xfd, 0x19, 0xbd} };
static const IID IID_IVisualTreeServiceCallback2_ = {
    0xbad9eb88, 0xae77, 0x4397, {0xb9, 0x48, 0x5f, 0xa2, 0xdb, 0x0a, 0x19, 0xea} };
static const IID IID_IXamlDiagnostics_ = {
    0x18c9e2b6, 0x3f43, 0x4116, {0x9f, 0x2b, 0xff, 0x93, 0x5d, 0x77, 0x70, 0xd2} };

static const wchar_t* kPipeName = L"\\\\.\\pipe\\lical-clockbar-a1";
static const wchar_t* kLogPath  = L"D:\\agents_tmp\\clockbar_tap.log";

// ── 日志：内存缓冲 + 后台落盘（回调线程只做拼接）─────
static SRWLOCK        g_logLock = SRWLOCK_INIT;
static std::string    g_logBuf;
static volatile LONG  g_logLines = 0;      // 总行数（含被丢弃的）
static const LONG     kMaxLoggedLines = 1500000;
static volatile LONG  g_flushQueued = 0;
static volatile LONG  g_objectsAlive = 0;
static volatile LONG  g_adviseCount = 0;   // 全进程累计 Advise 成功次数（重复订阅检测）
static volatile LONG  g_unadviseCount = 0;

static void log_line(const char* fmt, ...) {
    InterlockedIncrement(&g_logLines);
    if (g_logLines > kMaxLoggedLines) return;
    char tmp[1024];
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
    if (g_logBuf.size() > (4u << 20)) g_logBuf.erase(0, g_logBuf.size() - (1u << 20)); // 兜底
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
        HANDLE h = CreateFileW(kLogPath, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE,
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

// ── TAP 对象 ──────────────────────────────────────────
class TapObject;
static SRWLOCK   g_objLock = SRWLOCK_INIT;
static TapObject* g_cur = nullptr; // 最近活动对象（防重复订阅用，定义在类前）
class TapObject : public IObjectWithSite, public IVisualTreeServiceCallback2 {
    LONG                m_ref = 1;
    IXamlDiagnostics*   m_diag = nullptr;   // strong
    IVisualTreeService* m_svc = nullptr;    // strong
    bool                m_advised = false;
    SRWLOCK             m_lock = SRWLOCK_INIT;

    HRESULT AdviseLocked() {
        if (m_advised || !m_svc) return S_OK;
        LONG before = InterlockedIncrement(&g_adviseCount);
        HRESULT hr = m_svc->AdviseVisualTreeChange(
            static_cast<IVisualTreeServiceCallback*>(this));
        log_line("AdviseVisualTreeChange hr=0x%08lx (advises=%ld, tid=%lu)",
                 (unsigned long)hr, before, GetCurrentThreadId());
        m_advised = SUCCEEDED(hr);
        if (!m_advised) InterlockedDecrement(&g_adviseCount);
        return hr;
    }
    // TranslucentTB 实证：Advise 须从独立线程调（SetSite 线程内调用会挂/E_UNEXPECTED，
    // 且 Advise 被 special-case 到 UI 线程回调）。线程持强引用。
    void AdviseFromThread() {
        AddRef();
        HANDLE t = CreateThread(NULL, 0, [](LPVOID p) -> DWORD {
            TapObject* o = (TapObject*)p;
            try {
                AcquireSRWLockExclusive(&o->m_lock);
                o->AdviseLocked();
                ReleaseSRWLockExclusive(&o->m_lock);
            } catch (...) { log_line("advise thread EXCEPTION"); }
            o->Release();
            return 0;
        }, this, 0, NULL);
        if (t) CloseHandle(t); else Release();
    }
    void UnadviseLocked() {
        if (!m_advised) return;
        m_advised = false;
        if (m_svc) {
            InterlockedIncrement(&g_unadviseCount);
            HRESULT hr = m_svc->UnadviseVisualTreeChange(
                static_cast<IVisualTreeServiceCallback*>(this));
            log_line("UnadviseVisualTreeChange hr=0x%08lx", (unsigned long)hr);
        }
    }

public:
    TapObject() { InterlockedIncrement(&g_objectsAlive); log_line("TapObject ctor (alive=%ld)", g_objectsAlive); }
    ~TapObject() {
        AcquireSRWLockExclusive(&m_lock);
        UnadviseLocked();
        if (m_svc)  { m_svc->Release();  m_svc  = nullptr; }
        if (m_diag) { m_diag->Release(); m_diag = nullptr; }
        ReleaseSRWLockExclusive(&m_lock);
        InterlockedDecrement(&g_objectsAlive);
        log_line("TapObject dtor (alive=%ld)", g_objectsAlive);
    }

    // pipe 线程用的显式退订（A 阶段：vtable 直调，接受非 UI 线程；B 阶段改派发器投递）
    void UnadvisePublic() {
        AcquireSRWLockExclusive(&m_lock);
        UnadviseLocked();
        ReleaseSRWLockExclusive(&m_lock);
    }
    // 会话循环用：重新订阅（独立线程 Advise，TranslucentTB 实证要求）
    void AdvisePublic() { AdviseFromThread(); }

    // IUnknown（以 IObjectWithSite 分支为准）
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
            // 防重复订阅：新会话建立时先退订上一代对象（模块驻留 + 多次 init 时会累积）
            {
                AcquireSRWLockShared(&g_objLock); TapObject* prev = g_cur; ReleaseSRWLockShared(&g_objLock);
                if (prev && prev != this) prev->UnadvisePublic();
            }
            AcquireSRWLockExclusive(&m_lock);
            UnadviseLocked();
            if (m_diag) { m_diag->Release(); m_diag = nullptr; }
            if (m_svc)  { m_svc->Release();  m_svc  = nullptr; }
            HRESULT hr1 = pUnkSite->QueryInterface(IID_IXamlDiagnostics_, (void**)&m_diag);
            // 优先 IVisualTreeService3（ExplorerTAP 实证；vtable 前 slot 与 v1 兼容，Advise 走同一槽位）
            HRESULT hr2 = pUnkSite->QueryInterface(IID_IVisualTreeService3_, (void**)&m_svc);
            int svcVer = 3;
            if (FAILED(hr2)) {
                hr2 = pUnkSite->QueryInterface(IID_IVisualTreeService_, (void**)&m_svc);
                svcVer = 1;
            }
            log_line("SetSite: QI IXamlDiagnostics=0x%08lx IVisualTreeService(v%d)=0x%08lx",
                     (unsigned long)hr1, svcVer, (unsigned long)hr2);
            if (m_svc) AdviseFromThread();
            ReleaseSRWLockExclusive(&m_lock);
            return S_OK;
        } catch (...) { log_line("SetSite EXCEPTION"); return E_FAIL; }
    }
    STDMETHODIMP GetSite(REFIID riid, void** ppvSite) {
        if (!ppvSite) return E_POINTER;
        AcquireSRWLockShared(&m_lock);
        HRESULT hr = m_diag ? m_diag->QueryInterface(riid, ppvSite) : E_FAIL;
        ReleaseSRWLockShared(&m_lock);
        return hr;
    }

    // IVisualTreeServiceCallback（只读：不 free 引擎拥有的 BSTR；不碰树）
    STDMETHODIMP OnVisualTreeChange(ParentChildRelation relation, VisualElement element,
                                    VisualMutationType mutationType) {
        try {
            // 只读枚举；回调拥有 BSTR（TranslucentTB 同款语义），记录后释放
            char typeA[128] = "", nameA[128] = "";
            if (element.Type) WideCharToMultiByte(CP_UTF8, 0, element.Type, -1, typeA, sizeof(typeA), NULL, NULL);
            if (element.Name) WideCharToMultiByte(CP_UTF8, 0, element.Name, -1, nameA, sizeof(nameA), NULL, NULL);
            log_line("TREE %s h=%llX parent=%llX idx=%u type=%s name=%s nchild=%u",
                     mutationType == VisualMutationType::Add ? "ADD" : "REM",
                     (unsigned long long)element.Handle, (unsigned long long)relation.Parent,
                     relation.ChildIndex, typeA, nameA, element.NumChildren);
            if (mutationType == VisualMutationType::Add) {
                if (element.Type) SysFreeString(element.Type);
                if (element.Name) SysFreeString(element.Name);
                if (element.SrcInfo.FileName) SysFreeString(element.SrcInfo.FileName);
                if (element.SrcInfo.Hash) SysFreeString(element.SrcInfo.Hash);
            }
            return S_OK;
        } catch (...) { return S_OK; } // 躺平
    }
    // IVisualTreeServiceCallback2
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

// ── 最近活动对象注册（pipe 线程据此退订；不持强引用，仅配合存活计数观测）──
static void register_obj(TapObject* o) {
    AcquireSRWLockExclusive(&g_objLock); g_cur = o; ReleaseSRWLockExclusive(&g_objLock);
}

// 进程内初始化（TranslucentTB 生产通道实证配方）：
// · 每次尝试用【新线程】+【递增端点名 VisualDiagConnection{N}】——XAML Diagnostics 每线程
//   只能初始化一次，同线程重复调用静默返回 S_OK 什么都不做；
// · pid=GetCurrentProcessId()，wszDllXamlDiagnostics=nullptr，TAP dll=自身路径；
// · 失败重试 ≤60 次 × 500ms；成功后本 DLL 被 InitializeXamlDiagnosticsEx 永久钉在 explorer。
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

// DllMain（explorer 内）spawn：重试直到初始化成功
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

// 兼容旧 selfinit 命令（外部 attach 引导后进程内初始化，单次独立端点段）
static HRESULT SelfInit() {
    HRESULT hr = RunInitAttempt(1000);
    log_line("SelfInit hr=0x%08lx", (unsigned long)hr);
    return hr;
}

// ── pipe 客户端线程：host 控制命令（有界，异常即断开重连）──
#ifndef TAPVER
#define TAPVER 0
#endif
static void send_loaded(HANDLE h) {
    char out[192];
    _snprintf_s(out, sizeof(out), _TRUNCATE,
        "{\"v\":2,\"t\":\"loaded\",\"ver\":%d,\"advises\":%ld}\n", TAPVER, ((long)g_adviseCount));
    DWORD w = 0; WriteFile(h, out, (DWORD)strlen(out), &w, NULL);
}
static void send_line(HANDLE h, const char* s) {
    DWORD w = 0; WriteFile(h, s, (DWORD)strlen(s), &w, NULL);
}
static DWORD WINAPI pipe_thread(LPVOID) {
    log_line("pipe: client thread start");
    for (int retry = 0;;) {
        HANDLE h = CreateFileW(kPipeName, GENERIC_READ | GENERIC_WRITE, 0, NULL,
                               OPEN_EXISTING, 0, NULL);
        if (h == INVALID_HANDLE_VALUE) {
            if (retry < 3 || retry % 50 == 0)
                log_line("pipe: CreateFile failed gle=%lu retry=%d", GetLastError(), retry);
            if (++retry > 300) { log_line("pipe: server not present, keep waiting"); retry = 300; }
            Sleep(100); continue;
        }
        retry = 0;
        log_line("pipe: connected");
        send_loaded(h);
        char out[192];
        char buf[512];
        for (;;) {
            DWORD n = 0;
            BOOL ok = ReadFile(h, buf, sizeof(buf) - 1, &n, NULL);
            if (!ok) { log_line("pipe: ReadFile failed gle=%lu", GetLastError()); break; }
            if (n == 0) continue; // TRUE+0 = 虚唤醒（同服务端实测），连接仍活
            buf[n] = 0;
            if (strstr(buf, "unadvise")) {
                AcquireSRWLockShared(&g_objLock); TapObject* o = g_cur; ReleaseSRWLockShared(&g_objLock);
                if (o) o->UnadvisePublic(); else log_line("unadvise: no object");
                _snprintf_s(out, sizeof(out), _TRUNCATE,
                    "{\"v\":2,\"t\":\"unadvised\",\"advises\":%ld,\"unadvises\":%ld}\n",
                    ((long)g_adviseCount), ((long)g_unadviseCount));
                send_line(h, out);
            } else if (strstr(buf, "advise")) {
                AcquireSRWLockShared(&g_objLock); TapObject* o = g_cur; ReleaseSRWLockShared(&g_objLock);
                if (o) o->AdvisePublic(); else log_line("advise: no object");
                Sleep(200); // 等 Advise 线程落位后回报计数
                _snprintf_s(out, sizeof(out), _TRUNCATE,
                    "{\"v\":2,\"t\":\"advised\",\"advises\":%ld,\"unadvises\":%ld}\n",
                    ((long)g_adviseCount), ((long)g_unadviseCount));
                send_line(h, out);
            } else if (strstr(buf, "stats")) {
                _snprintf_s(out, sizeof(out), _TRUNCATE,
                    "{\"v\":2,\"t\":\"stats\",\"lines\":%ld,\"objs\":%ld,\"advises\":%ld,\"unadvises\":%ld}\n",
                    ((long)g_logLines), ((long)g_objectsAlive), ((long)g_adviseCount), ((long)g_unadviseCount));
                send_line(h, out);
            } else if (strstr(buf, "selfinit")) {
                HRESULT hr = SelfInit();
                char out2[96];
                _snprintf_s(out2, sizeof(out2), _TRUNCATE,
                    "{\"v\":2,\"t\":\"selfinit\",\"hr\":%lu}\n", (unsigned long)hr);
                send_line(h, out2);
            } else if (strstr(buf, "ping")) {
                send_line(h, "{\"v\":2,\"t\":\"pong\"}\n");
            }
        }
        CloseHandle(h);
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
    // 钩子注入通道（TranslucentTB 同款）：宿主经 SetWindowsHookEx(WH_CALLWNDPROC, LicalHookProc,
    // 本模块, 任务栏线程) 把本 DLL 载入 explorer；DllMain 里仅 CreateThread（允许），
    // 初始化全部在 InstallThread。仅当宿主是 explorer.exe 时启动（宿主自身 LoadLibrary 取
    // HMODULE 用于 SetWindowsHookEx，不得在宿主里跑初始化）。
    if (reason == DLL_PROCESS_ATTACH) {
        WCHAR exe[MAX_PATH]; DWORD n = GetModuleFileNameW(NULL, exe, MAX_PATH);
        if (n > 0) {
            const WCHAR* base = wcsrchr(exe, L'\\');
            base = base ? base + 1 : exe;
            if (_wcsicmp(base, L"explorer.exe") == 0) {
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
