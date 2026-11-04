/* Native WASAPI, CPU, and D3D11 work for the Windows HDA load acceptance test. */
#define COBJMACROS
#define INITGUID
#include <audioclient.h>
#include <avrt.h>
#include <d3d11.h>
#include <initguid.h>
#include <math.h>
#include <mmdeviceapi.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <windows.h>

static volatile LONG loading;
#define LEAD_FRAMES 48000u
#define SIGNAL_FRAMES 1440000u
#define TAIL_FRAMES 12000u

struct Period {
    unsigned long long timestamp;
    UINT32 offset, count;
    BYTE *buffer;
    HRESULT result;
};
static struct Period periods[8192];
static unsigned period_count;

static DWORD WINAPI cpu_load(void *unused) {
    (void)unused;
    volatile unsigned long value = 1;
    while (InterlockedCompareExchange(&loading, 0, 0)) {
        for (int i = 0; i < 10000; i++)
            value = value * 1664525 + 1013904223;
    }
    return 0;
}

static HRESULT fill(IAudioRenderClient *render, UINT32 count, UINT32 offset) {
    BYTE *bytes;
    HRESULT hr = IAudioRenderClient_GetBuffer(render, count, &bytes);
    if (FAILED(hr))
        return hr;
    short *samples = (short *)bytes;
    for (UINT32 i = 0; i < count; i++) {
        UINT32 absolute = offset + i;
        UINT32 position = absolute - LEAD_FRAMES;
        int signal = absolute >= LEAD_FRAMES && position < SIGNAL_FRAMES;
        samples[2 * i] =
            !signal ? 0 : (short)(6000 * sin(2 * 3.141592653589793 * 997 * position / 48000));
        samples[2 * i + 1] =
            signal && position % 48000 >= 12000 && position % 48000 < 12048 ? 6000 : 0;
    }
    FILETIME now;
    GetSystemTimePreciseAsFileTime(&now);
    unsigned long long timestamp =
        ((unsigned long long)now.dwHighDateTime << 32) | now.dwLowDateTime;
    hr = IAudioRenderClient_ReleaseBuffer(render, count, 0);
    if (period_count == sizeof(periods) / sizeof(periods[0]))
        return E_OUTOFMEMORY;
    periods[period_count++] = (struct Period){timestamp, offset, count, bytes, hr};
    return hr;
}

static int tone(void) {
    char log_path[MAX_PATH];
    DWORD path_length = GetTempPathA(MAX_PATH, log_path);
    if (!path_length || path_length + 24 >= MAX_PATH)
        return 1;
    strcat(log_path, "hda-tone-periods.csv");
    memset(periods, 0, sizeof(periods));
    period_count = 0;
    IMMDeviceEnumerator *enumerator = NULL;
    IMMDevice *device = NULL;
    IAudioClient *client = NULL;
    IAudioRenderClient *render = NULL;
    HRESULT hr = CoInitializeEx(NULL, COINIT_MULTITHREADED);
    if (FAILED(hr))
        return 1;
#define CHECK(call)                                                                                \
    do {                                                                                           \
        hr = (call);                                                                               \
        if (FAILED(hr)) {                                                                          \
            fprintf(stderr, "%s failed: %08lx\n", #call, (unsigned long)hr);                       \
            return 1;                                                                              \
        }                                                                                          \
    } while (0)
    CHECK(CoCreateInstance(&CLSID_MMDeviceEnumerator, NULL, CLSCTX_ALL, &IID_IMMDeviceEnumerator,
                           (void **)&enumerator));
    CHECK(IMMDeviceEnumerator_GetDefaultAudioEndpoint(enumerator, eRender, eConsole, &device));
    CHECK(IMMDevice_Activate(device, &IID_IAudioClient, CLSCTX_ALL, NULL, (void **)&client));
    WAVEFORMATEX format = {WAVE_FORMAT_PCM, 2, 48000, 192000, 4, 16, 0};
    REFERENCE_TIME period = 100000;
    CHECK(IAudioClient_Initialize(client, AUDCLNT_SHAREMODE_EXCLUSIVE,
                                  AUDCLNT_STREAMFLAGS_EVENTCALLBACK, period, period, &format,
                                  NULL));
    UINT32 capacity;
    CHECK(IAudioClient_GetBufferSize(client, &capacity));
    HANDLE event = CreateEventA(NULL, FALSE, FALSE, NULL);
    CHECK(IAudioClient_SetEventHandle(client, event));
    CHECK(IAudioClient_GetService(client, &IID_IAudioRenderClient, (void **)&render));
    DWORD task = 0;
    HANDLE scheduling = AvSetMmThreadCharacteristicsA("Pro Audio", &task);
    if (!scheduling || !AvSetMmThreadPriority(scheduling, AVRT_PRIORITY_CRITICAL))
        return 1;
    SYSTEM_INFO system;
    GetSystemInfo(&system);
    if (system.dwNumberOfProcessors > sizeof(DWORD_PTR) * 8)
        return 1;
    unsigned workers = system.dwNumberOfProcessors > 1 ? system.dwNumberOfProcessors - 1 : 1;
    unsigned audio_cpu = system.dwNumberOfProcessors - 1;
    if (!SetThreadAffinityMask(GetCurrentThread(), (DWORD_PTR)1 << audio_cpu))
        return 1;
    HANDLE *threads = calloc(workers, sizeof(HANDLE));
    if (!threads)
        return 1;
    InterlockedExchange(&loading, 1);
    for (unsigned i = 0; i < workers; i++) {
        threads[i] = CreateThread(NULL, 0, cpu_load, NULL, CREATE_SUSPENDED, NULL);
        if (!threads[i])
            return 1;
        if (!SetThreadAffinityMask(threads[i], (DWORD_PTR)1 << i))
            return 1;
        SetThreadPriority(threads[i], THREAD_PRIORITY_BELOW_NORMAL);
        ResumeThread(threads[i]);
    }
    CHECK(fill(render, capacity, 0));
    UINT32 written = capacity;
    CHECK(IAudioClient_Start(client));
    ULONGLONG start = GetTickCount64(), previous = start, max_gap = 0;
    while (written < LEAD_FRAMES + SIGNAL_FRAMES + TAIL_FRAMES) {
        if (WaitForSingleObject(event, 2000) != WAIT_OBJECT_0)
            return 1;
        ULONGLONG now = GetTickCount64();
        if (now - previous > max_gap)
            max_gap = now - previous;
        previous = now;
        CHECK(fill(render, capacity, written));
        written += capacity;
    }
    if (WaitForSingleObject(event, 2000) != WAIT_OBJECT_0)
        return 1;
    printf("Native WASAPI capacity=%u frames, workers=%u, audio_cpu=%u, elapsed=%llu ms, max "
           "gap=%llu ms\n",
           capacity, workers, audio_cpu, GetTickCount64() - start, max_gap);
    CHECK(IAudioClient_Stop(client));
    FILE *period_log = fopen(log_path, "w");
    if (!period_log)
        return 1;
    for (unsigned i = 0; i < period_count; i++) {
        const struct Period *p = &periods[i];
        fprintf(period_log, "%llu,%u,%u,%p,%08lx\n", p->timestamp, p->offset, p->count, p->buffer,
                (unsigned long)p->result);
    }
    if (fclose(period_log) != 0)
        return 1;
    InterlockedExchange(&loading, 0);
    for (unsigned i = 0; i < workers; i++) {
        WaitForSingleObject(threads[i], INFINITE);
        CloseHandle(threads[i]);
    }
    free(threads);
    AvRevertMmThreadCharacteristics(scheduling);
    CloseHandle(event);
    IAudioRenderClient_Release(render);
    IAudioClient_Release(client);
    IMMDevice_Release(device);
    IMMDeviceEnumerator_Release(enumerator);
    CoUninitialize();
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 2)
        return 2;
    if (strcmp(argv[1], "--tone") == 0)
        return tone();
    ID3D11Device *device = NULL;
    ID3D11DeviceContext *context = NULL;
    D3D_FEATURE_LEVEL level;
    HRESULT hr = D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, NULL, 0,
                                   D3D11_SDK_VERSION, &device, &level, &context);
    if (FAILED(hr)) {
        fprintf(stderr, "D3D11 hardware device failed: %08lx\n", (unsigned long)hr);
        return 1;
    }
    D3D11_TEXTURE2D_DESC description = {0};
    description.Width = 1920;
    description.Height = 1080;
    description.MipLevels = description.ArraySize = 1;
    description.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    description.SampleDesc.Count = 1;
    description.BindFlags = D3D11_BIND_RENDER_TARGET;
    ID3D11Texture2D *texture = NULL;
    hr = ID3D11Device_CreateTexture2D(device, &description, NULL, &texture);
    if (FAILED(hr))
        return 1;
    ID3D11RenderTargetView *target = NULL;
    hr = ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)texture, NULL, &target);
    if (FAILED(hr))
        return 1;
    D3D11_QUERY_DESC query_description = {D3D11_QUERY_EVENT, 0};
    ID3D11Query *query = NULL;
    hr = ID3D11Device_CreateQuery(device, &query_description, &query);
    if (FAILED(hr))
        return 1;
    printf("D3D11 hardware load ready, feature level=%x\n", level);
    fflush(stdout);
    ULONGLONG deadline = GetTickCount64() + 90000;
    unsigned long batches = 0;
    while (GetFileAttributesA(argv[1]) == INVALID_FILE_ATTRIBUTES && GetTickCount64() < deadline) {
        for (int i = 0; i < 64; i++) {
            FLOAT colour[4] = {(FLOAT)(batches % 256) / 255, (FLOAT)i / 63, 0.5f, 1};
            ID3D11DeviceContext_ClearRenderTargetView(context, target, colour);
        }
        ID3D11DeviceContext_End(context, (ID3D11Asynchronous *)query);
        ID3D11DeviceContext_Flush(context);
        while ((hr = ID3D11DeviceContext_GetData(context, (ID3D11Asynchronous *)query, NULL, 0,
                                                 0)) == S_FALSE) {
            if (GetTickCount64() > deadline)
                return 1;
            Sleep(1);
        }
        if (FAILED(hr))
            return 1;
        batches++;
    }
    printf("D3D11 completed batches=%lu\n", batches);
    ID3D11Query_Release(query);
    ID3D11RenderTargetView_Release(target);
    ID3D11Texture2D_Release(texture);
    ID3D11DeviceContext_Release(context);
    ID3D11Device_Release(device);
    return batches == 0;
}
