# Verify the actual Windows shell-facing executable icons, not PNG encodings.
param(
    [Parameter(Mandatory = $true)][string]$Cli,
    [Parameter(Mandatory = $true)][string]$Gui,
    [string]$Logo = (Join-Path (Split-Path -Parent $PSScriptRoot) 'assets/pixel-art-logo.png')
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$expected = [System.Drawing.Bitmap]::new([System.IO.Path]::GetFullPath($Logo))
try {
    foreach ($path in @($Cli, $Gui)) {
        $icon = [System.Drawing.Icon]::ExtractAssociatedIcon([System.IO.Path]::GetFullPath($path))
        if ($null -eq $icon) { throw "Windows shell returned no icon for $path" }
        try {
            $actual = $icon.ToBitmap()
            try {
                if ($actual.Width -ne $expected.Width -or $actual.Height -ne $expected.Height) {
                    throw "Windows shell icon dimensions do not match original logo: $path ($($actual.Width)x$($actual.Height))"
                }
                for ($y = 0; $y -lt $expected.Height; $y++) {
                    for ($x = 0; $x -lt $expected.Width; $x++) {
                        $a = $actual.GetPixel($x, $y)
                        $e = $expected.GetPixel($x, $y)
                        if ($a.ToArgb() -ne $e.ToArgb() -and -not ($a.A -eq 0 -and $e.A -eq 0)) {
                            throw "Windows shell icon does not match original logo at ${x},${y}: $path"
                        }
                    }
                }
                Write-Host "$path : Windows shell-extracted icon matches original logo pixels"
            }
            finally { $actual.Dispose() }
        }
        finally { $icon.Dispose() }
    }
}
finally { $expected.Dispose() }
