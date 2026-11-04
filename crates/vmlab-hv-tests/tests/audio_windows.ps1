param([switch]$Quality, [switch]$Load)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Threading;

[ComImport, Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface DeviceEnumerator {
    void EnumAudioEndpoints(int flow, uint mask, out IntPtr devices);
    void GetDefaultAudioEndpoint(int flow, int role, out AudioDevice device);
}
[ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface AudioDevice {
    void Activate(ref Guid iid, uint context, IntPtr parameters,
        [MarshalAs(UnmanagedType.IUnknown)] out object instance);
}
[ComImport, Guid("1CB9AD4C-DBFA-4C32-B178-C2F568A703B2"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface AudioClient {
    void Initialize(int mode, uint flags, long duration, long period, IntPtr format, IntPtr session);
    void GetBufferSize(out uint frames);
    void GetStreamLatency(out long latency);
    void GetCurrentPadding(out uint frames);
    [PreserveSig] int IsFormatSupported(int mode, IntPtr format, out IntPtr closest);
    void GetMixFormat(out IntPtr format);
    void GetDevicePeriod(out long defaultPeriod, out long minimumPeriod);
    void Start();
    void Stop();
    void Reset();
    void SetEventHandle(IntPtr handle);
    void GetService(ref Guid iid, [MarshalAs(UnmanagedType.IUnknown)] out object service);
}
[ComImport, Guid("F294ACFC-3146-4483-A7BF-ADDCA7C260E2"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface RenderClient {
    void GetBuffer(uint frames, out IntPtr data);
    void ReleaseBuffer(uint frames, uint flags);
}

public static class HdaTone {
    [DllImport("avrt.dll", CharSet = CharSet.Unicode)]
    static extern IntPtr AvSetMmThreadCharacteristics(string taskName, out uint taskIndex);
    [DllImport("avrt.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    static extern bool AvRevertMmThreadCharacteristics(IntPtr handle);

    [StructLayout(LayoutKind.Sequential, Pack = 2)]
    struct WaveFormat {
        public ushort tag, channels;
        public uint rate, bytesPerSecond;
        public ushort alignment, bits, extra;
    }

    static void Fill(RenderClient render, uint frames, uint offset, bool quality) {
        IntPtr destination;
        render.GetBuffer(frames, out destination);
        var samples = new short[frames * 2];
        for (uint frame = 0; frame < frames; frame++) {
            uint position = offset + frame;
            short sample = quality && position >= 480000 ? (short)0
                : (short)((quality ? 6000 : 300) * Math.Sin(2 * Math.PI * (quality ? 1000 : 440) * position / 48000));
            samples[frame * 2] = sample;
            samples[frame * 2 + 1] = quality
                ? (short)(position < 480000 && position % 48000 >= 12000 && position % 48000 < 12048 ? 6000 : 0)
                : sample;
        }
        Marshal.Copy(samples, 0, destination, samples.Length);
        render.ReleaseBuffer(frames, 0);
    }

    public static void Play(bool quality) {
        var enumerator = (DeviceEnumerator)Activator.CreateInstance(Type.GetTypeFromCLSID(
            new Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")));
        AudioDevice device;
        enumerator.GetDefaultAudioEndpoint(0, 0, out device);
        object instance;
        var iid = typeof(AudioClient).GUID;
        device.Activate(ref iid, 23, IntPtr.Zero, out instance);
        var client = (AudioClient)instance;
        EventWaitHandle samplesReady = null;
        IntPtr format = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(WaveFormat)));
        try {
            Marshal.StructureToPtr(new WaveFormat {
                tag = 1, channels = 2, rate = 48000, bytesPerSecond = 192000,
                alignment = 4, bits = 16
            }, format, false);
            long period = 1000000;
            uint flags = quality ? 0x40000u : 0u;
            try {
                client.Initialize(1, flags, period, quality ? period : 0, format, IntPtr.Zero);
            } catch (COMException error) {
                if (!quality || error.ErrorCode != unchecked((int)0x88890019)) throw;
                uint aligned;
                client.GetBufferSize(out aligned);
                period = (long)(10000000.0 * aligned / 48000 + 0.5);
                Marshal.ReleaseComObject(client);
                iid = typeof(AudioClient).GUID;
                device.Activate(ref iid, 23, IntPtr.Zero, out instance);
                client = (AudioClient)instance;
                client.Initialize(1, flags, period, period, format, IntPtr.Zero);
            }
            if (quality) {
                samplesReady = new EventWaitHandle(false, EventResetMode.AutoReset);
                client.SetEventHandle(samplesReady.SafeWaitHandle.DangerousGetHandle());
            }
            uint capacity;
            client.GetBufferSize(out capacity);
            iid = typeof(RenderClient).GUID;
            client.GetService(ref iid, out instance);
            var render = (RenderClient)instance;
            uint total = quality ? 492000u : 144000u;
            uint written = Math.Min(capacity, total);
            Fill(render, written, 0, quality);
            uint taskIndex;
            IntPtr scheduling = AvSetMmThreadCharacteristics("Pro Audio", out taskIndex);
            if (scheduling == IntPtr.Zero) throw new Exception("MMCSS audio scheduling unavailable");
            client.Start();
            try {
                var clock = System.Diagnostics.Stopwatch.StartNew();
                long previous = 0, maxGap = 0;
                var deadline = DateTime.UtcNow.AddSeconds(30);
                while (true) {
                    if (quality && !samplesReady.WaitOne(2000)) throw new Exception("WASAPI render event timed out");
                    long now = clock.ElapsedTicks;
                    maxGap = Math.Max(maxGap, now - previous);
                    previous = now;
                    if (quality) {
                        if (written >= total) break;
                        Fill(render, capacity, written, true);
                        written += capacity;
                        continue;
                    }
                    uint padding;
                    client.GetCurrentPadding(out padding);
                    if (written == total && padding == 0) break;
                    if (DateTime.UtcNow > deadline) throw new Exception("WASAPI playback timed out");
                    uint count = Math.Min(capacity - padding, total - written);
                    if (count != 0) {
                        Fill(render, count, written, quality);
                        written += count;
                    }
                    Thread.Sleep(5);
                }
                Console.WriteLine("WASAPI capacity={0} frames, max service gap={1:F3} ms, elapsed={2:F6} s", capacity,
                    maxGap * 1000.0 / System.Diagnostics.Stopwatch.Frequency, clock.Elapsed.TotalSeconds);
            } finally {
                client.Stop();
                AvRevertMmThreadCharacteristics(scheduling);
                Marshal.ReleaseComObject(render);
            }
        } finally {
            Marshal.FreeHGlobal(format);
            Marshal.ReleaseComObject(client);
            if (samplesReady != null) samplesReady.Dispose();
            Marshal.ReleaseComObject(device);
            Marshal.ReleaseComObject(enumerator);
        }
    }
}
'@
$gpu = $null
$stop = "$env:TEMP\hda-gpu-stop"
$gpuLog = "$env:TEMP\hda-gpu.log"
try {
    if ($Load) {
        if (-not (Test-Path "$env:TEMP\hda-gpu-load.exe")) { throw 'Native HDA load executable missing' }
        Remove-Item $stop -ErrorAction SilentlyContinue
        $gpu = Start-Process "$env:TEMP\hda-gpu-load.exe" -ArgumentList $stop -PassThru -RedirectStandardOutput $gpuLog -RedirectStandardError "$gpuLog.err"
        $null = $gpu.Handle
        for ($n = 0; $n -lt 300; $n++) {
            if ((Test-Path $gpuLog) -and (Get-Content $gpuLog -Raw) -match 'load ready') { break }
            if ($gpu.HasExited) { throw "D3D11 load failed: $(Get-Content \"$gpuLog.err\")" }
            Start-Sleep -Milliseconds 100
        }
        if ($n -eq 300) { throw 'D3D11 load startup timed out' }
    }
    if ($Load) {
        & "$env:TEMP\hda-gpu-load.exe" --tone
        if ($LASTEXITCODE -ne 0) { throw "Native WASAPI load exit $LASTEXITCODE" }
    } else {
        [HdaTone]::Play($Quality.IsPresent)
    }
} finally {
    if ($null -ne $gpu) {
        if ($gpu.HasExited) { throw 'D3D11 load ended before playback completed' }
        [IO.File]::WriteAllText($stop, 'stop')
        if (-not $gpu.WaitForExit(10000)) { $gpu.Kill(); throw 'D3D11 load stop timed out' }
        Get-Content $gpuLog
        if ($gpu.ExitCode -ne 0) { throw "D3D11 load exit $($gpu.ExitCode)" }
    }
}
