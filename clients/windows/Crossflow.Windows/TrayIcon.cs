using System.Runtime.InteropServices;
using Microsoft.UI.Xaml;
using Microsoft.Win32;
using WinRT.Interop;

namespace Crossflow.Windows;

internal enum TrayVisualState
{
    Searching,
    Connected,
    ActiveRemote,
    Paused
}

/// Native Win32 system tray icon attached to MainWindow's HWND via SetWindowSubclass.
/// Features 4 anti-aliased visual status icons (auto-adapting to light/dark taskbar),
/// full ShareMouse-grade context menu (quick toggles, channel submenu, send files,
/// receive folder, startup auto-launch), and automatic TaskbarCreated recovery.
internal sealed class TrayIcon : IDisposable
{
    private const uint WM_APP = 0x8000;
    private const uint WM_TRAYICON = WM_APP + 101;
    private const uint WM_SETTINGCHANGE = 0x001A;
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
    private const uint MF_GRAYED = 0x00000001;
    private const uint MF_CHECKED = 0x00000008;
    private const uint MF_POPUP = 0x00000010;
    private const uint MF_SEPARATOR = 0x00000800;
    private const uint TPM_RIGHTBUTTON = 0x0002;
    private const uint TPM_RETURNCMD = 0x0100;
    private const uint TPM_NONOTIFY = 0x0080;

    private const uint CMD_SHOW_HIDE = 1001;
    private const uint CMD_TOGGLE_SHARE = 1002;
    private const uint CMD_TOGGLE_CLIPBOARD = 1003;
    private const uint CMD_TOGGLE_INPUT_STATE = 1004;
    private const uint CMD_SEND_FILES = 1005;
    private const uint CMD_OPEN_RECEIVE_DIR = 1006;
    private const uint CMD_TOGGLE_AUTOSTART = 1007;
    private const uint CMD_EXIT = 1008;
    private const uint CMD_TOGGLE_SCREEN_LOCK = 1009;
    private const uint CMD_QJ_NEXT = 1010;
    private const uint CMD_QJ_HOME = 1011;
    private const uint CMD_SEND_FILES_ALL = 1012;
    private const uint CMD_CHANNEL_BASE = 2000;
    private const uint CMD_QJ_PEER_BASE = 3000;
    private const uint CMD_SEND_PEER_BASE = 4000;
    private const uint CMD_PEER_CHANNEL_BASE = 5000; // stride 64 per peer

    private const nuint SUBCLASS_ID = 0x4346_5452;

    private readonly IntPtr hwnd;
    private readonly uint taskbarCreatedMsg;
    private readonly SubclassProc subclassDelegate;
    private readonly Func<bool> isWindowVisible;
    private readonly Dictionary<(TrayVisualState, bool), IntPtr> iconCache = new();
    private IntPtr currentIcon;
    private bool lightTaskbar;
    private TrayVisualState visualState = TrayVisualState.Searching;
    private bool added;
    private bool disposed;

    private string tooltip = "Crossflow";
    private string statusLine = "状态：正在发现设备";
    private string? focusLine;
    private bool canToggleSharing;
    private bool isSharing;
    private bool isConnected;
    private bool clipboardEnabled = true;
    private bool inputStateEnabled = true;
    private bool screenLocked;
    private IReadOnlyList<PeerInfo> connectedPeers = [];
    private string selectedChannel = "auto";
    private IReadOnlyList<NetworkChannelInfo> channels = [];

    internal event Action? ShowOrRestoreRequested;
    internal event Action? HideRequested;
    internal event Action? ToggleSharingRequested;
    internal event Action<bool>? ToggleScreenLockRequested;
    internal event Action<string?>? QuickJumpRequested;
    internal event Action<bool>? ToggleClipboardRequested;
    internal event Action<bool>? ToggleInputStateRequested;
    internal event Action<string>? SelectChannelRequested;
    internal event Action<string, string>? SelectPeerChannelRequested;
    internal event Action<string?>? SendFilesDialogRequested;
    internal event Action? OpenReceiveFolderRequested;
    internal event Action? ExitRequested;

    internal TrayIcon(Window window, Func<bool> isWindowVisible)
    {
        this.isWindowVisible = isWindowVisible;
        hwnd = WindowNative.GetWindowHandle(window);
        taskbarCreatedMsg = RegisterWindowMessageW("TaskbarCreated");
        lightTaskbar = IsSystemLightTaskbar();
        currentIcon = GetOrCreateIcon(visualState, lightTaskbar);
        subclassDelegate = WindowProc;
        SetWindowSubclass(hwnd, subclassDelegate, SUBCLASS_ID, IntPtr.Zero);
        AddIcon();
    }

    internal void UpdateState(
        TrayVisualState state,
        string tip,
        string primaryStatus,
        string? secondaryFocus,
        bool connected,
        bool canToggle,
        bool sharing,
        bool locked,
        IReadOnlyList<PeerInfo> activePeers,
        bool clipboardSync,
        bool inputStateSync,
        string channelId,
        IReadOnlyList<NetworkChannelInfo> availableChannels)
    {
        if (disposed) return;
        bool themeNow = IsSystemLightTaskbar();
        bool iconChanged = state != visualState || themeNow != lightTaskbar;
        visualState = state;
        lightTaskbar = themeNow;
        if (iconChanged)
            currentIcon = GetOrCreateIcon(visualState, lightTaskbar);

        string nextTip = tip.Length > 120 ? tip[..120] : tip;
        bool tipChanged = nextTip != tooltip;
        tooltip = nextTip;
        statusLine = primaryStatus;
        focusLine = secondaryFocus;
        isConnected = connected;
        canToggleSharing = canToggle;
        isSharing = sharing;
        screenLocked = locked;
        connectedPeers = activePeers;
        clipboardEnabled = clipboardSync;
        inputStateEnabled = inputStateSync;
        selectedChannel = channelId;
        channels = availableChannels;

        if (!added) AddIcon();
        else if (iconChanged || tipChanged) ModifyIcon();
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
        hIcon = currentIcon,
        szTip = tooltip
    };

    private IntPtr WindowProc(IntPtr hWnd, uint uMsg, UIntPtr wParam, IntPtr lParam, nuint uIdSubclass, IntPtr dwRefData)
    {
        if (uMsg == WM_TRAYICON)
        {
            uint mouseMsg = (uint)(lParam.ToInt64() & 0xFFFF);
            if (mouseMsg is WM_LBUTTONUP or WM_LBUTTONDBLCLK)
            {
                if (isWindowVisible()) HideRequested?.Invoke();
                else ShowOrRestoreRequested?.Invoke();
                return IntPtr.Zero;
            }
            if (mouseMsg is WM_RBUTTONUP or WM_CONTEXTMENU)
            {
                ShowContextMenu();
                return IntPtr.Zero;
            }
        }
        else if (uMsg == WM_SETTINGCHANGE)
        {
            bool nextLight = IsSystemLightTaskbar();
            if (nextLight != lightTaskbar)
            {
                lightTaskbar = nextLight;
                currentIcon = GetOrCreateIcon(visualState, lightTaskbar);
                if (added) ModifyIcon();
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
        var submenus = new List<IntPtr>();
        if (menu == IntPtr.Zero) return;
        try
        {
            bool visible = isWindowVisible();
            AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, statusLine);
            if (!string.IsNullOrWhiteSpace(focusLine))
                AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, focusLine);
            AppendMenuW(menu, MF_SEPARATOR, 0, null);

            AppendMenuW(menu, MF_STRING, CMD_SHOW_HIDE, visible ? "隐藏到托盘" : "打开主界面");
            if (canToggleSharing)
                AppendMenuW(menu, MF_STRING, CMD_TOGGLE_SHARE, isSharing ? "暂停共享" : "恢复共享");

            if (isSharing && connectedPeers.Count > 0)
            {
                IntPtr qjMenu = CreatePopupMenu();
                submenus.Add(qjMenu);
                AppendMenuW(qjMenu, MF_STRING, CMD_QJ_NEXT, "切到下一台设备");
                AppendMenuW(qjMenu, MF_STRING, CMD_QJ_HOME, "回到本机");
                AppendMenuW(qjMenu, MF_SEPARATOR, 0, null);
                for (int i = 0; i < connectedPeers.Count && i < 16; i++)
                {
                    var p = connectedPeers[i];
                    int slot = p.Slot ?? (i + 1);
                    uint flags = MF_STRING | (p.Focused ? MF_CHECKED : 0);
                    AppendMenuW(qjMenu, flags, CMD_QJ_PEER_BASE + (uint)i, $"#{slot} {p.Name}");
                }
                AppendMenuW(menu, MF_POPUP, (nuint)qjMenu, "快速切换 (QuickJump)");
            }

            AppendMenuW(menu, MF_SEPARATOR, 0, null);
            AppendMenuW(menu, MF_STRING | (screenLocked ? MF_CHECKED : 0), CMD_TOGGLE_SCREEN_LOCK, "锁定当前屏幕（防误触）");
            AppendMenuW(menu, MF_STRING | (clipboardEnabled ? MF_CHECKED : 0), CMD_TOGGLE_CLIPBOARD, "剪贴板共享");
            AppendMenuW(menu, MF_STRING | (inputStateEnabled ? MF_CHECKED : 0), CMD_TOGGLE_INPUT_STATE, "中英文与大小写同步");

            if (channels.Count > 0)
            {
                IntPtr channelMenu = CreatePopupMenu();
                submenus.Add(channelMenu);
                AppendMenuW(channelMenu, MF_STRING | MF_GRAYED, 0, "全局默认信道");
                for (int i = 0; i < channels.Count && i < 32; i++)
                {
                    var ch = channels[i];
                    uint flags = MF_STRING | (string.Equals(ch.Id, selectedChannel, StringComparison.Ordinal) ? MF_CHECKED : 0);
                    AppendMenuW(channelMenu, flags, CMD_CHANNEL_BASE + (uint)i, ch.Label);
                }
                if (connectedPeers.Count > 0)
                {
                    AppendMenuW(channelMenu, MF_SEPARATOR, 0, null);
                    for (int pi = 0; pi < connectedPeers.Count && pi < 16; pi++)
                    {
                        var peer = connectedPeers[pi];
                        IntPtr peerChMenu = CreatePopupMenu();
                        submenus.Add(peerChMenu);
                        for (int ci = 0; ci < channels.Count && ci < 32; ci++)
                        {
                            var ch = channels[ci];
                            string label = ci == 0 ? "跟随全局默认" : ch.Label;
                            uint flags = MF_STRING | (string.Equals(ch.Id, peer.PreferredChannel, StringComparison.Ordinal) ? MF_CHECKED : 0);
                            AppendMenuW(peerChMenu, flags, CMD_PEER_CHANNEL_BASE + (uint)(pi * 64 + ci), label);
                        }
                        string activeTag = string.IsNullOrWhiteSpace(peer.ActiveChannelLabel) ? "" : $" ({peer.ActiveChannelLabel})";
                        AppendMenuW(channelMenu, MF_POPUP, (nuint)peerChMenu, $"{peer.Name}{activeTag}");
                    }
                }
                AppendMenuW(menu, MF_POPUP, (nuint)channelMenu, "网络信道");
            }

            AppendMenuW(menu, MF_SEPARATOR, 0, null);
            if (isConnected && connectedPeers.Count > 0)
            {
                IntPtr sendMenu = CreatePopupMenu();
                submenus.Add(sendMenu);
                for (int i = 0; i < connectedPeers.Count && i < 16; i++)
                {
                    var p = connectedPeers[i];
                    AppendMenuW(sendMenu, MF_STRING, CMD_SEND_PEER_BASE + (uint)i, $"发送给 {p.Name}…");
                }
                if (connectedPeers.Count > 1)
                {
                    AppendMenuW(sendMenu, MF_SEPARATOR, 0, null);
                    AppendMenuW(sendMenu, MF_STRING, CMD_SEND_FILES_ALL, "发送给全部已连接设备…");
                }
                AppendMenuW(menu, MF_POPUP, (nuint)sendMenu, "发送文件到");
            }
            else
            {
                AppendMenuW(menu, MF_STRING | MF_GRAYED, CMD_SEND_FILES, "发送文件到对端…");
            }
            AppendMenuW(menu, MF_STRING, CMD_OPEN_RECEIVE_DIR, "打开接收文件夹");

            AppendMenuW(menu, MF_SEPARATOR, 0, null);
            bool autoStart = IsAutoStartEnabled();
            AppendMenuW(menu, MF_STRING | (autoStart ? MF_CHECKED : 0), CMD_TOGGLE_AUTOSTART, "开机自启动");
            AppendMenuW(menu, MF_STRING, CMD_EXIT, "退出 Crossflow");

            GetCursorPos(out var pt);
            SetForegroundWindow(hwnd);
            uint cmd = TrackPopupMenuEx(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY, pt.X, pt.Y, hwnd, IntPtr.Zero);
            PostMessageW(hwnd, 0, UIntPtr.Zero, IntPtr.Zero);

            if (cmd >= CMD_CHANNEL_BASE && cmd < CMD_CHANNEL_BASE + (uint)channels.Count)
            {
                SelectChannelRequested?.Invoke(channels[(int)(cmd - CMD_CHANNEL_BASE)].Id);
                return;
            }
            if (cmd >= CMD_QJ_PEER_BASE && cmd < CMD_QJ_PEER_BASE + (uint)connectedPeers.Count)
            {
                QuickJumpRequested?.Invoke(connectedPeers[(int)(cmd - CMD_QJ_PEER_BASE)].Id);
                return;
            }
            if (cmd >= CMD_SEND_PEER_BASE && cmd < CMD_SEND_PEER_BASE + (uint)connectedPeers.Count)
            {
                SendFilesDialogRequested?.Invoke(connectedPeers[(int)(cmd - CMD_SEND_PEER_BASE)].Id);
                return;
            }
            if (cmd >= CMD_PEER_CHANNEL_BASE && cmd < CMD_PEER_CHANNEL_BASE + 16 * 64)
            {
                int offset = (int)(cmd - CMD_PEER_CHANNEL_BASE);
                int pi = offset / 64;
                int ci = offset % 64;
                if (pi < connectedPeers.Count && ci < channels.Count)
                    SelectPeerChannelRequested?.Invoke(connectedPeers[pi].Id, channels[ci].Id);
                return;
            }
            switch (cmd)
            {
                case CMD_SHOW_HIDE:
                    if (visible) HideRequested?.Invoke();
                    else ShowOrRestoreRequested?.Invoke();
                    break;
                case CMD_TOGGLE_SHARE:
                    ToggleSharingRequested?.Invoke();
                    break;
                case CMD_TOGGLE_SCREEN_LOCK:
                    ToggleScreenLockRequested?.Invoke(!screenLocked);
                    break;
                case CMD_QJ_NEXT:
                    QuickJumpRequested?.Invoke(null);
                    break;
                case CMD_QJ_HOME:
                    QuickJumpRequested?.Invoke("local");
                    break;
                case CMD_SEND_FILES_ALL:
                    SendFilesDialogRequested?.Invoke(null);
                    break;
                case CMD_TOGGLE_CLIPBOARD:
                    ToggleClipboardRequested?.Invoke(!clipboardEnabled);
                    break;
                case CMD_TOGGLE_INPUT_STATE:
                    ToggleInputStateRequested?.Invoke(!inputStateEnabled);
                    break;
                case CMD_OPEN_RECEIVE_DIR:
                    OpenReceiveFolderRequested?.Invoke();
                    break;
                case CMD_TOGGLE_AUTOSTART:
                    SetAutoStartEnabled(!autoStart);
                    break;
                case CMD_EXIT:
                    ExitRequested?.Invoke();
                    break;
            }
        }
        finally
        {
            foreach (var sub in submenus)
                if (sub != IntPtr.Zero) DestroyMenu(sub);
            DestroyMenu(menu);
        }
    }

    internal static bool IsAutoStartEnabled()
    {
        try
        {
            using var key = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Run", false);
            return key?.GetValue("Crossflow") is string v && !string.IsNullOrWhiteSpace(v);
        }
        catch { return false; }
    }

    internal static void SetAutoStartEnabled(bool enable)
    {
        try
        {
            using var key = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Run", true);
            if (key is null) return;
            if (enable)
            {
                string? exe = Environment.ProcessPath;
                if (!string.IsNullOrWhiteSpace(exe))
                    key.SetValue("Crossflow", $"\"{exe}\" --minimized");
            }
            else
            {
                key.DeleteValue("Crossflow", false);
            }
        }
        catch { }
    }

    private static bool IsSystemLightTaskbar()
    {
        try
        {
            using var key = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", false);
            return key?.GetValue("SystemUsesLightTheme") is int v && v != 0;
        }
        catch { return false; }
    }

    private IntPtr GetOrCreateIcon(TrayVisualState state, bool light)
    {
        if (iconCache.TryGetValue((state, light), out var cached)) return cached;
        IntPtr created = RenderDualScreenTrayIcon(state, light);
        iconCache[(state, light)] = created;
        return created;
    }

    /// 2x supersampled (64x64 -> 32x32) anti-aliased vector rendering of the Crossflow
    /// dual-screen + crossing-arrow tray glyph with 4 status themes.
    private static IntPtr RenderDualScreenTrayIcon(TrayVisualState state, bool lightTaskbar)
    {
        const int ss = 64;
        const int size = 32;
        var hi = new uint[ss * ss];

        uint glyphColor = state switch
        {
            TrayVisualState.Connected => lightTaskbar ? 0xFF1E293Bu : 0xFFF8FAFCu,
            TrayVisualState.ActiveRemote => lightTaskbar ? 0xFF2563EBu : 0xFF38BDF8u,
            TrayVisualState.Searching => lightTaskbar ? 0xFF64748Bu : 0xFF94A3B8u,
            TrayVisualState.Paused => lightTaskbar ? 0xFF64748Bu : 0xFF94A3B8u,
            _ => 0xFFF8FAFCu
        };

        // 1. Dual rounded rectangular monitor frames (left: x 4..28, right: x 36..60, y 10..54, stroke 5px)
        DrawRoundedRectStroke(hi, ss, 4, 10, 28, 54, 7, 5, glyphColor);
        DrawRoundedRectStroke(hi, ss, 36, 10, 60, 54, 7, 5, glyphColor);

        // Clear a crisp portal gap across the inner vertical edges where the crossing arrow bridges
        FillRect(hi, ss, 20, 22, 44, 42, 0x00000000u);

        // 2. Bold horizontal crossing arrow shaft + chevron head bridging left -> right
        FillRoundedRect(hi, ss, 15, 29, 44, 35, 3, glyphColor);
        // Arrow head tip at (48, 32)
        for (int y = 21; y <= 43; y++)
        {
            int dy = Math.Abs(y - 32);
            int xStart = 35 + dy - 4;
            int xEnd = 49 - dy;
            for (int x = Math.Max(34, xStart); x <= Math.Min(49, xEnd); x++)
            {
                if (x >= 35 + dy - 5 && x <= 49 - dy)
                    hi[y * ss + x] = glyphColor;
            }
        }

        // 3. Status badges in bottom-right corner
        if (state == TrayVisualState.Connected)
        {
            // Clean halo cutout + emerald green dot (#16A34A)
            FillCircle(hi, ss, 53, 53, 11, 0x00000000u);
            FillCircle(hi, ss, 53, 53, 8, 0xFF16A34Au);
        }
        else if (state == TrayVisualState.Paused)
        {
            // Clean halo cutout + amber circle (#D97706) + white pause bars
            FillCircle(hi, ss, 52, 52, 12, 0x00000000u);
            FillCircle(hi, ss, 52, 52, 10, 0xFFD97706u);
            FillRect(hi, ss, 48, 47, 50, 57, 0xFFFFFFFFu);
            FillRect(hi, ss, 54, 47, 56, 57, 0xFFFFFFFFu);
        }

        // 2x2 box downsample with premultiplied alpha for crisp anti-aliased 32x32 output
        var pixels = new uint[size * size];
        for (int y = 0; y < size; y++)
        {
            for (int x = 0; x < size; x++)
            {
                int r = 0, g = 0, b = 0, a = 0;
                for (int sy = 0; sy < 2; sy++)
                {
                    for (int sx = 0; sx < 2; sx++)
                    {
                        uint p = hi[(y * 2 + sy) * ss + (x * 2 + sx)];
                        int pa = (int)(p >> 24);
                        a += pa;
                        r += (int)((p >> 16) & 0xFF) * pa;
                        g += (int)((p >> 8) & 0xFF) * pa;
                        b += (int)(p & 0xFF) * pa;
                    }
                }
                int outA = a / 4;
                if (outA == 0) pixels[y * size + x] = 0;
                else pixels[y * size + x] = ((uint)outA << 24) | ((uint)(r / a) << 16) | ((uint)(g / a) << 8) | (uint)(b / a);
            }
        }

        var bmi = new BITMAPV5HEADER
        {
            bV5Size = (uint)Marshal.SizeOf<BITMAPV5HEADER>(),
            bV5Width = size,
            bV5Height = -size,
            bV5Planes = 1,
            bV5BitCount = 32,
            bV5Compression = 3,
            bV5RedMask = 0x00FF0000,
            bV5GreenMask = 0x0000FF00,
            bV5BlueMask = 0x000000FF,
            bV5AlphaMask = 0xFF000000
        };

        IntPtr hdc = GetDC(IntPtr.Zero);
        IntPtr hbmColor = CreateDIBSection(hdc, ref bmi, 0, out IntPtr bits, IntPtr.Zero, 0);
        ReleaseDC(IntPtr.Zero, hdc);
        if (hbmColor == IntPtr.Zero || bits == IntPtr.Zero)
            return LoadIconW(IntPtr.Zero, (IntPtr)32512);

        Marshal.Copy(Array.ConvertAll(pixels, u => unchecked((int)u)), 0, bits, pixels.Length);
        IntPtr hbmMask = CreateBitmap(size, size, 1, 1, IntPtr.Zero);
        var info = new ICONINFO { fIcon = true, hbmMask = hbmMask, hbmColor = hbmColor };
        IntPtr icon = CreateIconIndirect(ref info);
        DeleteObject(hbmColor);
        if (hbmMask != IntPtr.Zero) DeleteObject(hbmMask);
        return icon != IntPtr.Zero ? icon : LoadIconW(IntPtr.Zero, (IntPtr)32512);
    }

    private static void DrawRoundedRectStroke(uint[] buf, int s, int l, int t, int r, int b, int rad, int stroke, uint color)
    {
        for (int y = t; y <= b; y++)
            for (int x = l; x <= r; x++)
                if (InsideRoundedRect(x, y, l, t, r, b, rad)
                    && !InsideRoundedRect(x, y, l + stroke, t + stroke, r - stroke, b - stroke, Math.Max(1, rad - stroke)))
                    buf[y * s + x] = color;
    }

    private static void FillRoundedRect(uint[] buf, int s, int l, int t, int r, int b, int rad, uint color)
    {
        for (int y = t; y <= b; y++)
            for (int x = l; x <= r; x++)
                if (InsideRoundedRect(x, y, l, t, r, b, rad))
                    buf[y * s + x] = color;
    }

    private static void FillRect(uint[] buf, int s, int l, int t, int r, int b, uint color)
    {
        for (int y = Math.Max(0, t); y <= Math.Min(s - 1, b); y++)
            for (int x = Math.Max(0, l); x <= Math.Min(s - 1, r); x++)
                buf[y * s + x] = color;
    }

    private static void FillCircle(uint[] buf, int s, int cx, int cy, int rad, uint color)
    {
        int r2 = rad * rad;
        for (int y = Math.Max(0, cy - rad); y <= Math.Min(s - 1, cy + rad); y++)
            for (int x = Math.Max(0, cx - rad); x <= Math.Min(s - 1, cx + rad); x++)
                if ((x - cx) * (x - cx) + (y - cy) * (y - cy) <= r2)
                    buf[y * s + x] = color;
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
        foreach (var icon in iconCache.Values)
            if (icon != IntPtr.Zero) DestroyIcon(icon);
        iconCache.Clear();
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
    private static extern bool AppendMenuW(IntPtr hMenu, uint uFlags, nuint uIDNewItem, string? lpNewItem);

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
