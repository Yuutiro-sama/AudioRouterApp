using System;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Runtime.InteropServices;

namespace AudioRouterApp;

static class TrayIconFactory
{
    [DllImport("user32.dll")]
    private static extern bool DestroyIcon(IntPtr handle);

    public static Icon Create()
    {
        using var bmp = new Bitmap(32, 32);
        using (var g = Graphics.FromImage(bmp))
        {
            g.SmoothingMode = SmoothingMode.AntiAlias;
            g.Clear(Color.Transparent);

            using var bg = new SolidBrush(Color.FromArgb(255, 0, 120, 215));
            g.FillEllipse(bg, 1, 1, 30, 30);

            using var white = new SolidBrush(Color.White);
            g.FillRectangle(white, 7, 12, 4, 8);
            g.FillPolygon(white, new[]
            {
                new Point(11, 12), new Point(17, 7), new Point(17, 25), new Point(11, 20)
            });

            using var pen = new Pen(Color.White, 2f);
            g.DrawArc(pen, 14, 11, 8, 10, -50, 100);
            g.DrawArc(pen, 14, 8, 12, 16, -50, 100);
        }

        IntPtr handle = bmp.GetHicon();
        try
        {
            using var temp = Icon.FromHandle(handle);
            return (Icon)temp.Clone();
        }
        finally
        {
            DestroyIcon(handle); // giải phóng bộ nhớ của Windows
        }
    }
}