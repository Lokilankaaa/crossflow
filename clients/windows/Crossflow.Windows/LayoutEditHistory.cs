namespace Crossflow.Windows;

internal sealed class LayoutEditHistory
{
    private readonly List<LayoutDocument> undo = [], redo = [];
    internal bool CanUndo => undo.Count > 0;
    internal bool CanRedo => redo.Count > 0;
    internal void Clear() { undo.Clear(); redo.Clear(); }
    internal LayoutDocument? Peek(bool forward) => (forward ? redo : undo).LastOrDefault();
    internal void Record(LayoutDocument previous) { Push(undo, previous); redo.Clear(); }
    internal void Complete(bool forward, LayoutDocument previous)
    {
        var from = forward ? redo : undo;
        if (from.Count == 0) throw new InvalidOperationException("没有可恢复的排列。");
        from.RemoveAt(from.Count - 1); Push(forward ? undo : redo, previous);
    }
    private static void Push(List<LayoutDocument> stack, LayoutDocument value)
    {
        if (stack.Count == 50) stack.RemoveAt(0);
        stack.Add(new(value.Screens.ToList()));
    }
}
