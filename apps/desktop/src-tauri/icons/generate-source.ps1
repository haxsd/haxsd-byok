# Draws the icons for haxsd byok.
#
# The mark is a request path through a local gateway: three connected nodes on a
# deep indigo tile. It deliberately avoids key/credential imagery, which reads as
# security software rather than a model-routing tool.
#
# Run this from the icons directory. It writes the 1024px source plus the four
# linux-*.png names that bundle.icon references, because `tauri icon` does not know
# about those legacy names and would leave them on the previous artwork. The
# remaining sizes come from:
#
#   npm --prefix ../.. run tauri -- icon <absolute path to icon-source.png>
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

# Route: a smooth path from the lower-left node through the local gateway node to
# the upper-right node. Round nodes and caps keep the mark legible at 32px.
$glyphStart = [System.Drawing.Color]::FromArgb(255, 143, 174, 255)
$glyphEnd = [System.Drawing.Color]::FromArgb(255, 114, 217, 203)
$routeBrush = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
    (New-Object System.Drawing.Point(300, 716)),
    (New-Object System.Drawing.Point(724, 308)),
    $glyphStart,
    $glyphEnd)
$routePen = New-Object System.Drawing.Pen($routeBrush, 76)
$routePen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
$routePen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
$routePen.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round
$route = New-Object System.Drawing.Drawing2D.GraphicsPath
$route.AddBezier(300, 716, 390, 716, 388, 512, 512, 512)
$route.AddBezier(512, 512, 636, 512, 634, 308, 724, 308)
$graphics.DrawPath($routePen, $route)

$startNode = New-Object System.Drawing.SolidBrush($glyphStart)
$endNode = New-Object System.Drawing.SolidBrush($glyphEnd)
$graphics.FillEllipse($startNode, 236, 652, 128, 128)
$graphics.FillEllipse($endNode, 660, 244, 128, 128)

# The middle node is open: it marks the local gateway the path passes through.
$gatewayFill = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(255, 17, 24, 45))
$graphics.FillEllipse($routeBrush, 395, 395, 234, 234)
$graphics.FillEllipse($gatewayFill, 457, 457, 110, 110)

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
