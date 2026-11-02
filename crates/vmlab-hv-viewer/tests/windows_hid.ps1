$ErrorActionPreference = 'Stop'
Add-Type @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

public static class ViewerHid {
    [StructLayout(LayoutKind.Explicit, Size = 64)]
    private struct Caps {
        [FieldOffset(0)] public ushort Usage;
        [FieldOffset(2)] public ushort UsagePage;
        [FieldOffset(4)] public ushort InputReportLength;
    }
    [StructLayout(LayoutKind.Sequential)]
    private struct InterfaceData {
        public int Size;
        public Guid ClassGuid;
        public int Flags;
        public IntPtr Reserved;
    }
    [DllImport("hid.dll")]
    private static extern void HidD_GetHidGuid(out Guid guid);
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr SetupDiGetClassDevs(ref Guid guid, IntPtr enumerator, IntPtr parent, uint flags);
    [DllImport("setupapi.dll", SetLastError = true)]
    private static extern bool SetupDiEnumDeviceInterfaces(IntPtr set, IntPtr device, ref Guid guid, uint index, ref InterfaceData data);
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool SetupDiGetDeviceInterfaceDetail(IntPtr set, ref InterfaceData data, IntPtr detail, uint size, out uint required, IntPtr device);
    [DllImport("setupapi.dll")]
    private static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern SafeFileHandle CreateFile(string name, uint access, uint share, IntPtr security, uint creation, uint flags, IntPtr template);
    [DllImport("hid.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.U1)]
    private static extern bool HidD_GetPreparsedData(SafeFileHandle device, out IntPtr data);
    [DllImport("hid.dll")]
    [return: MarshalAs(UnmanagedType.U1)]
    private static extern bool HidD_FreePreparsedData(IntPtr data);
    [DllImport("hid.dll")]
    private static extern int HidP_GetCaps(IntPtr data, out Caps caps);

    public static ushort[] Capabilities() {
        Guid guid;
        HidD_GetHidGuid(out guid);
        IntPtr set = SetupDiGetClassDevs(ref guid, IntPtr.Zero, IntPtr.Zero, 0x12);
        if (set == new IntPtr(-1)) throw new Win32Exception();
        try {
            for (uint index = 0; ; index++) {
                InterfaceData data = new InterfaceData();
                data.Size = Marshal.SizeOf(typeof(InterfaceData));
                if (!SetupDiEnumDeviceInterfaces(set, IntPtr.Zero, ref guid, index, ref data)) {
                    if (Marshal.GetLastWin32Error() == 259) break;
                    throw new Win32Exception();
                }
                uint required;
                SetupDiGetDeviceInterfaceDetail(set, ref data, IntPtr.Zero, 0, out required, IntPtr.Zero);
                if (required < 8 || required > 65536) throw new InvalidOperationException("Invalid HID interface size");
                IntPtr detail = Marshal.AllocHGlobal((int)required);
                try {
                    Marshal.WriteInt32(detail, IntPtr.Size == 8 ? 8 : 6);
                    if (!SetupDiGetDeviceInterfaceDetail(set, ref data, detail, required, out required, IntPtr.Zero))
                        throw new Win32Exception();
                    string path = Marshal.PtrToStringUni(IntPtr.Add(detail, 4));
                    if (path.IndexOf("vid_0627&pid_0001", StringComparison.OrdinalIgnoreCase) < 0) continue;
                    using (SafeFileHandle device = CreateFile(path, 0, 3, IntPtr.Zero, 3, 0, IntPtr.Zero)) {
                        if (device.IsInvalid) throw new Win32Exception();
                        IntPtr preparsed;
                        if (!HidD_GetPreparsedData(device, out preparsed)) throw new Win32Exception();
                        try {
                            Caps caps;
                            int status = HidP_GetCaps(preparsed, out caps);
                            if (status != 0x110000) throw new InvalidOperationException("HidP_GetCaps failed: " + status);
                            return new ushort[] { caps.Usage, caps.UsagePage, caps.InputReportLength };
                        } finally {
                            HidD_FreePreparsedData(preparsed);
                        }
                    }
                } finally {
                    Marshal.FreeHGlobal(detail);
                }
            }
        } finally {
            SetupDiDestroyDeviceInfoList(set);
        }
        throw new InvalidOperationException("QEMU USB tablet HID interface not found");
    }
}
'@
