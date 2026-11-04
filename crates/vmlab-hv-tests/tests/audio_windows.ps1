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
    [StructLayout(LayoutKind.Sequential, Pack = 2)]
    struct WaveFormat {
        public ushort tag, channels;
        public uint rate, bytesPerSecond;
        public ushort alignment, bits, extra;
    }

    static void Fill(RenderClient render, uint frames, uint offset) {
        IntPtr destination;
        render.GetBuffer(frames, out destination);
        var samples = new short[frames * 2];
        for (uint frame = 0; frame < frames; frame++) {
            short sample = (short)(300 * Math.Sin(2 * Math.PI * 440 * (offset + frame) / 48000));
            samples[frame * 2] = samples[frame * 2 + 1] = sample;
        }
        Marshal.Copy(samples, 0, destination, samples.Length);
        render.ReleaseBuffer(frames, 0);
    }

    public static void Play() {
        var enumerator = (DeviceEnumerator)Activator.CreateInstance(Type.GetTypeFromCLSID(
            new Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")));
        AudioDevice device;
        enumerator.GetDefaultAudioEndpoint(0, 0, out device);
        object instance;
        var iid = typeof(AudioClient).GUID;
        device.Activate(ref iid, 23, IntPtr.Zero, out instance);
        var client = (AudioClient)instance;
        IntPtr format = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(WaveFormat)));
        try {
            Marshal.StructureToPtr(new WaveFormat {
                tag = 1, channels = 2, rate = 48000, bytesPerSecond = 192000,
                alignment = 4, bits = 16
            }, format, false);
            client.Initialize(1, 0, 1000000, 0, format, IntPtr.Zero);
            uint capacity;
            client.GetBufferSize(out capacity);
            iid = typeof(RenderClient).GUID;
            client.GetService(ref iid, out instance);
            var render = (RenderClient)instance;
            uint written = Math.Min(capacity, 144000);
            Fill(render, written, 0);
            client.Start();
            try {
                var deadline = DateTime.UtcNow.AddSeconds(15);
                while (true) {
                    uint padding;
                    client.GetCurrentPadding(out padding);
                    if (written == 144000 && padding == 0) break;
                    if (DateTime.UtcNow > deadline) throw new Exception("WASAPI playback timed out");
                    uint count = Math.Min(capacity - padding, 144000 - written);
                    if (count != 0) {
                        Fill(render, count, written);
                        written += count;
                    }
                    Thread.Sleep(5);
                }
            } finally {
                client.Stop();
                Marshal.ReleaseComObject(render);
            }
        } finally {
            Marshal.FreeHGlobal(format);
            Marshal.ReleaseComObject(client);
            Marshal.ReleaseComObject(device);
            Marshal.ReleaseComObject(enumerator);
        }
    }
}
'@
[HdaTone]::Play()
