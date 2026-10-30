namespace Crossflow.Windows;

/// Routed capture-loss events from a descendant are not loss of canvas capture.
internal sealed class DragCaptureOwnership
{
    private uint? pointer;
    internal void Begin(uint pointerId) => pointer = pointerId;
    internal bool Matches(uint pointerId) => pointer == pointerId;
    internal void End() => pointer = null;
    internal bool Lost(object source, object canvas, uint pointerId)
    {
        if (!ReferenceEquals(source, canvas) || !Matches(pointerId)) return false;
        End(); return true;
    }
}
