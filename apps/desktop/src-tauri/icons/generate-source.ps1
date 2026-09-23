# Draws the icons for haxsd byok.
#
# The product shares no visual identity with the sibling Cursor BYOK product, and
# that is deliberate rather than cosmetic: the two shipped byte-identical icons, so
# in the taskbar and in a window list there was no way to tell which application was
# which. This one is a key on a deep indigo tile, drawn with the midnight theme's
# accent colour.
#
# Run this from the icons directory. It writes the 1024px source plus the four
# linux-*.png names that bundle.icon references, because `tauri icon` does not know
# about those legacy names and would leave them on the previous artwork. The
# remaining sizes come from:
#
#   npm --prefix ../../../ run tauri icon -- <absolute path to icon-source.png>
#
# usage: powershell -File generate-source.ps1

Add-Type -AssemblyName System.Drawing

$size = 1024
$bitmap = New-Object System.Drawing.Bitmap($size, $size)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
$graphics.Clear([System.Drawing.Color]::Transparent)

# Tile: a rounded square in the midnight palette.
$inset = 40
$tile = New-Object System.Drawing.Rectangle($inset, $inset, ($size - 2 * $inset), ($size - 2 * $inset))
$radius = 210
$tilePath = New-Object System.Drawing.Drawing2D.GraphicsPath
$diameter = 2 * $radius
$tilePath.AddArc($tile.X, $tile.Y, $diameter, $diameter, 180, 90)
$tilePath.AddArc(($tile.Right - $diameter), $tile.Y, $diameter, $diameter, 270, 90)
$tilePath.AddArc(($tile.Right - $diameter), ($tile.Bottom - $diameter), $diameter, $diameter, 0, 90)
$tilePath.AddArc($tile.X, ($tile.Bottom - $diameter), $diameter, $diameter, 90, 90)
$tilePath.CloseFigure()

$tileBrush = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
    (New-Object System.Drawing.Point(0, $inset)),
    (New-Object System.Drawing.Point(0, ($size - $inset))),
    [System.Drawing.Color]::FromArgb(255, 32, 42, 77),
    [System.Drawing.Color]::FromArgb(255, 13, 18, 32))
$graphics.FillPath($tileBrush, $tilePath)

$borderPen = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(40, 255, 255, 255), 6)
$graphics.DrawPath($borderPen, $tilePath)

# Key: a ring, a shaft and two teeth, stroked with round caps so the shape still
# reads once it is scaled down to a taskbar entry.
$glyph = [System.Drawing.Color]::FromArgb(255, 124, 156, 255)
$ringPen = New-Object System.Drawing.Pen($glyph, 82)
$ringPen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
$ringPen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
$ringRadius = 135
$graphics.DrawEllipse($ringPen, (372 - $ringRadius), (512 - $ringRadius), (2 * $ringRadius), (2 * $ringRadius))

$shaft = New-Object System.Drawing.Pen($glyph, 82)
$shaft.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
$shaft.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
$graphics.DrawLine($shaft, 482, 512, 792, 512)

$tooth = New-Object System.Drawing.Pen($glyph, 58)
$tooth.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
$tooth.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
$graphics.DrawLine($tooth, 726, 512, 726, 632)
$graphics.DrawLine($tooth, 634, 512, 634, 588)

$graphics.Dispose()
$bitmap.Save((Join-Path (Get-Location) "icon-source.png"), [System.Drawing.Imaging.ImageFormat]::Png)
Write-Host "wrote icon-source.png"

# bundle.icon points at the Tauri v1 names, which `tauri icon` never writes.
foreach ($edge in 32, 128, 256, 512) {
    $scaled = New-Object System.Drawing.Bitmap($edge, $edge)
    $target = [System.Drawing.Graphics]::FromImage($scaled)
    $target.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $target.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $target.DrawImage($bitmap, 0, 0, $edge, $edge)
    $target.Dispose()
    $name = "linux-${edge}x${edge}.png"
    $scaled.Save((Join-Path (Get-Location) $name), [System.Drawing.Imaging.ImageFormat]::Png)
    $scaled.Dispose()
    Write-Host "wrote $name"
}

$bitmap.Dispose()
