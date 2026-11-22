using System.Runtime.InteropServices;
using Microsoft.UI.Xaml;
using WinRT.Interop;

namespace Crossflow.Windows;

/// Native Win32 system tray icon attached to MainWindow's HWND via SetWindowSubclass.
/// Zero external package dependencies; supports left-click restore, right-click native
/// context menu, dynamic status updates, and automatic TaskbarCreated recovery.
internal sealed class TrayIcon : IDisposable
{
    private const uint WM_APP = 0x8000;
    private const uint WM_TRAYICON = WM_APP + 101;
    private const uint WM_LBUTTONUP = 0x0202;
    private const uint WM_LBUTTONDBLCLK = 0x0203;
    private const uint WM_RBUTTONUP = 0x0205;
    private const uint WM_CONTEXTMENU = 0x007B;

    private const uint NIM_ADD = 0x00000000;
    private const uint NIM_MODIFY = 0x00000001;
    private const uint NIM_DELETE = 0x00000002;
    private const uint NIF_MESSAGE = 0x00000001;
    private const uint NIF_ICON = 0x00000002;
    private const uint NIF_TIP = 0x00000004;

    private const uint MF_STRING = 0x00000000;
    private const uint MF_SEPARATOR = 0x00000800;
    private const uint MF_GRAYED = 0x00000001;
    private const uint TPM_RIGHTBUTTON = 0x0002;
    private const uint TPM_RETURNCMD = 0x0100;
    private const uint TPM_NONOTIFY = 0x0080;

    private const uint CMD_SHOW_HIDE = 1001;
    private const uint CMD_TOGGLE_SHARE = 1002;
    private const uint CMD_EXIT = 1003;

    private const nuint SUBCLASS_ID = 0x4346_5452;

    private readonly IntPtr hwnd;
    private readonly uint taskbarCreatedMsg;
    private readonly SubclassProc subclassDelegate;
    private IntPtr hIcon;
    private bool added;
    private bool disposed;

    private string tooltip = "Crossflow";
    private string statusLabel = "Crossflow · 正在发现设备";
    private bool canToggleSharing;
    private bool isSharing;
    private Func<bool>? isWindowVisible;

    internal event Action? ShowOrRestoreRequested;
    internal event Action? HideRequested;
    internal event Action? ToggleSharingRequested;
    internal event Action? ExitRequested;

    internal TrayIcon(Window window, Func<bool> isWindowVisible)
    {
        this.isWindowVisible = isWindowVisible;
        hwnd = WindowNative.GetWindowHandle(window);
        taskbarCreatedMsg = RegisterWindowMessageW("TaskbarCreated");
        hIcon = CreateCrossflowIcon();
        subclassDelegate = WindowProc;
        SetWindowSubclass(hwnd, subclassDelegate, SUBCLASS_ID, IntPtr.Zero);
        AddIcon();
    }

    internal void UpdateStatus(string tip, string menuStatus, bool canToggle, bool sharing)
    {
        if (disposed) return;
        tooltip = tip.Length > 120 ? tip[..120] : tip;
        statusLabel = menuStatus;
        canToggleSharing = canToggle;
        isSharing = sharing;
        if (!added) AddIcon();
        else ModifyIcon();
    }

    private void AddIcon()
    {
        var nid = BuildData();
        added = Shell_NotifyIconW(NIM_ADD, ref nid);
    }

    private void ModifyIcon()
    {
        var nid = BuildData();
        if (!Shell_NotifyIconW(NIM_MODIFY, ref nid))
            added = Shell_NotifyIconW(NIM_ADD, ref nid);
    }

    private NOTIFYICONDATAW BuildData() => new()
    {
        cbSize = (uint)Marshal.SizeOf<NOTIFYICONDATAW>(),
        hWnd = hwnd,
        uID = 1,
        uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage = WM_TRAYICON,
        hIcon = hIcon,
        szTip = tooltip
    };

    private IntPtr WindowProc(IntPtr hWnd, uint uMsg, UIntPtr wParam, IntPtr lParam, nuint uIdSubclass, IntPtr dwRefData)
    {
        if (uMsg == WM_TRAYICON)
        {
            uint mouseMsg = (uint)(lParam.ToInt64() & 0xFFFF);
            if (mouseMsg is WM_LBUTTONUP or WM_LBUTTONDBLCLK)
            {
                ShowOrRestoreRequested?.Invoke();
                return IntPtr.Zero;
            }
            if (mouseMsg is WM_RBUTTONUP or WM_CONTEXTMENU)
            {
                ShowContextMenu();
                return IntPtr.Zero;
            }
        }
        else if (taskbarCreatedMsg != 0 && uMsg == taskbarCreatedMsg)
        {
            AddIcon();
        }
        return DefSubclassProc(hWnd, uMsg, wParam, lParam);
    }

    private void ShowContextMenu()
    {
        IntPtr menu = CreatePopupMenu();
        if (menu == IntPtr.Zero) return;
        try
        {
            bool visible = isWindowVisible?.Invoke() ?? true;
            AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, statusLabel);
            AppendMenuW(menu, MF_SEPARATOR, 0, null);
            AppendMenuW(menu, MF_STRING, CMD_SHOW_HIDE, visible ? "隐藏到托盘" : "显示主界面");
            if (canToggleSharing)
                AppendMenuW(menu, MF_STRING, CMD_TOGGLE_SHARE, isSharing ? "暂停共享" : "恢复共享");
            AppendMenuW(menu, MF_SEPARATOR, 0, null);
            AppendMenuW(menu, MF_STRING, CMD_EXIT, "退出 Crossflow");

            GetCursorPos(out var pt);
            SetForegroundWindow(hwnd);
            uint cmd = TrackPopupMenuEx(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY, pt.X, pt.Y, hwnd, IntPtr.Zero);
            PostMessageW(hwnd, 0, UIntPtr.Zero, IntPtr.Zero); // dismiss menu cleanly per MSDN

            switch (cmd)
            {
                case CMD_SHOW_HIDE:
                    if (visible) HideRequested?.Invoke();
                    else ShowOrRestoreRequested?.Invoke();
                    break;
                case CMD_TOGGLE_SHARE:
                    ToggleSharingRequested?.Invoke();
                    break;
                case CMD_EXIT:
                    ExitRequested?.Invoke();
                    break;
            }
        }
        finally
        {
            DestroyMenu(menu);
        }
    }

    public void Dispose()
    {
        if (disposed) return;
        disposed = true;
        if (added)
        {
            var nid = new NOTIFYICONDATAW
            {
                cbSize = (uint)Marshal.SizeOf<NOTIFYICONDATAW>(),
                hWnd = hwnd,
                uID = 1
            };
            Shell_NotifyIconW(NIM_DELETE, ref nid);
            added = false;
        }
        RemoveWindowSubclass(hwnd, subclassDelegate, SUBCLASS_ID);
        if (hIcon != IntPtr.Zero)
        {
            DestroyIcon(hIcon);
            hIcon = IntPtr.Zero;
        }
    }

    /// Build a crisp 32x32 ARGB tray icon (Crossflow blue rounded badge with dual screens).
    private static IntPtr CreateCrossflowIcon()
    {
        const int size = 32;
        var pixels = new uint[size * size];
        for (int y = 0; y < size; y++)
        {
            for (int x = 0; x < size; x++)
            {
                // Rounded square background (#2563EB) with 6px corner radius
                bool inside = InsideRoundedRect(x, y, 1, 1, 30, 30, 7);
                if (!inside)
                {
                    pixels[y * size + x] = 0x00000000;
                    continue;
                }
                // Two clean white screen glyphs side-by-side
                bool leftScreen = (x >= 5 && x <= 14 && y >= 9 && y <= 20)
                    && !(x >= 7 && x <= 12 && y >= 11 && y <= 18);
                bool leftStand = x >= 8 && x <= 11 && y >= 22 && y <= 23;
                bool rightScreen = (x >= 17 && x <= 26 && y >= 9 && y <= 20)
                    && !(x >= 19 && x <= 24 && y >= 11 && y <= 18);
                bool rightStand = x >= 20 && x <= 23 && y >= 22 && y <= 23;
                pixels[y * size + x] = (leftScreen || leftStand || rightScreen || rightStand)
                    ? 0xFFFFFFFFu
                    : 0xFF2563EBu;
            }
        }

        var bmi = new BITMAPV5HEADER
        {
            bV5Size = (uint)Marshal.SizeOf<BITMAPV5HEADER>(),
            bV5Width = size,
            bV5Height = -size, // top-down
            bV5Planes = 1,
            bV5BitCount = 32,
            bV5Compression = 3, // BI_BITFIELDS
            bV5RedMask = 0x00FF0000,
            bV5GreenMask = 0x0000FF00,
            bV5BlueMask = 0x000000FF,
            bV5AlphaMask = 0xFF000000
        };

        IntPtr hdc = GetDC(IntPtr.Zero);
        IntPtr hbmColor = CreateDIBSection(hdc, ref bmi, 0, out IntPtr bits, IntPtr.Zero, 0);
        ReleaseDC(IntPtr.Zero, hdc);
        if (hbmColor == IntPtr.Zero || bits == IntPtr.Zero)
            return LoadIconW(IntPtr.Zero, (IntPtr)32512); // IDI_APPLICATION fallback

        Marshal.Copy(Array.ConvertAll(pixels, u => unchecked((int)u)), 0, bits, pixels.Length);
        IntPtr hbmMask = CreateBitmap(size, size, 1, 1, IntPtr.Zero);
        var info = new ICONINFO { fIcon = true, hbmMask = hbmMask, hbmColor = hbmColor };
        IntPtr icon = CreateIconIndirect(ref info);
        DeleteObject(hbmColor);
        if (hbmMask != IntPtr.Zero) DeleteObject(hbmMask);
        return icon != IntPtr.Zero ? icon : LoadIconW(IntPtr.Zero, (IntPtr)32512);
    }

    private static bool InsideRoundedRect(int x, int y, int left, int top, int right, int bottom, int r)
    {
        if (x < left || x > right || y < top || y > bottom) return false;
        int dx = 0, dy = 0;
        if (x < left + r) dx = left + r - x;
        else if (x > right - r) dx = x - (right - r);
        if (y < top + r) dy = top + r - y;
        else if (y > bottom - r) dy = y - (bottom - r);
        return dx * dx + dy * dy <= r * r;
    }

    private delegate IntPtr SubclassProc(IntPtr hWnd, uint uMsg, UIntPtr wParam, IntPtr lParam, nuint uIdSubclass, IntPtr dwRefData);

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct NOTIFYICONDATAW
    {
        public uint cbSize;
        public IntPtr hWnd;
        public uint uID;
        public uint uFlags;
        public uint uCallbackMessage;
        public IntPtr hIcon;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)]
        public string szTip;
        public uint dwState;
        public uint dwStateMask;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)]
        public string szInfo;
        public uint uVersion;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 64)]
        public string szInfoTitle;
        public uint dwInfoFlags;
        public Guid guidItem;
        public IntPtr hBalloonIcon;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct POINT { public int X; public int Y; }

    [StructLayout(LayoutKind.Sequential)]
    private struct ICONINFO
    {
        [MarshalAs(UnmanagedType.Bool)] public bool fIcon;
        public int xHotspot;
        public int yHotspot;
        public IntPtr hbmMask;
        public IntPtr hbmColor;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct BITMAPV5HEADER
    {
        public uint bV5Size;
        public int bV5Width;
        public int bV5Height;
        public ushort bV5Planes;
        public ushort bV5BitCount;
        public uint bV5Compression;
        public uint bV5SizeImage;
        public int bV5XPelsPerMeter;
        public int bV5YPelsPerMeter;
        public uint bV5ClrUsed;
        public uint bV5ClrImportant;
        public uint bV5RedMask;
        public uint bV5GreenMask;
        public uint bV5BlueMask;
        public uint bV5AlphaMask;
        public uint bV5CSType;
        public long bV5Endpoints0, bV5Endpoints1, bV5Endpoints2, bV5Endpoints3;
        public int bV5Endpoints4;
        public uint bV5GammaRed;
        public uint bV5GammaGreen;
        public uint bV5GammaBlue;
        public uint bV5Intent;
        public uint bV5ProfileData;
        public uint bV5ProfileSize;
        public uint bV5Reserved;
    }

    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool Shell_NotifyIconW(uint dwMessage, ref NOTIFYICONDATAW lpData);

    [DllImport("comctl32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetWindowSubclass(IntPtr hWnd, SubclassProc pfnSubclass, nuint uIdSubclass, IntPtr dwRefData);

    [DllImport("comctl32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool RemoveWindowSubclass(IntPtr hWnd, SubclassProc pfnSubclass, nuint uIdSubclass);

    [DllImport("comctl32.dll")]
    private static extern IntPtr DefSubclassProc(IntPtr hWnd, uint uMsg, UIntPtr wParam, IntPtr lParam);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern uint RegisterWindowMessageW(string lpString);

    [DllImport("user32.dll")]
    private static extern IntPtr CreatePopupMenu();

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool AppendMenuW(IntPtr hMenu, uint uFlags, uint uIDNewItem, string? lpNewItem);

    [DllImport("user32.dll")]
    private static extern uint TrackPopupMenuEx(IntPtr hMenu, uint uFlags, int x, int y, IntPtr hwnd, IntPtr lptpm);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool DestroyMenu(IntPtr hMenu);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetCursorPos(out POINT lpPoint);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetForegroundWindow(IntPtr hWnd);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool PostMessageW(IntPtr hWnd, uint msg, UIntPtr wParam, IntPtr lParam);

    [DllImport("user32.dll")]
    private static extern IntPtr GetDC(IntPtr hWnd);

    [DllImport("user32.dll")]
    private static extern int ReleaseDC(IntPtr hWnd, IntPtr hDC);

    [DllImport("user32.dll")]
    private static extern IntPtr CreateIconIndirect(ref ICONINFO piconinfo);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool DestroyIcon(IntPtr hIcon);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr LoadIconW(IntPtr hInstance, IntPtr lpIconName);

    [DllImport("gdi32.dll")]
    private static extern IntPtr CreateDIBSection(IntPtr hdc, ref BITMAPV5HEADER pbmi, uint usage, out IntPtr ppvBits, IntPtr hSection, uint offset);

    [DllImport("gdi32.dll")]
    private static extern IntPtr CreateBitmap(int nWidth, int nHeight, uint nPlanes, uint nBitCount, IntPtr lpBits);

    [DllImport("gdi32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool DeleteObject(IntPtr ho);
}
