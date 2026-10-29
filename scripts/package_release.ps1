# package_release.ps1
# 编译 Release 版本并打包为 Windows 免安装压缩包（用于 GitHub Releases）。
#
# - 通过 --remap-path-prefix 去除可执行文件中嵌入的本机绝对路径（如用户目录），避免泄露个人信息；
# - 需要先运行 scripts\sync_open_lexicons.ps1 生成 data\compiled\merged_lexicon.bin。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File scripts\package_release.ps1 -Version 0.1.0

param(
    [string]$Version = "0.1.0",
    [string]$TargetDir = "C:\rust-build\rust-ai-pinyin-ime-dist"
)

$ErrorActionPreference = "Stop"
$RootDir = Split-Path -Parent $PSScriptRoot
$Lexicon = Join-Path $RootDir "data\compiled\merged_lexicon.bin"
if (-not (Test-Path $Lexicon)) {
    throw "未找到 $Lexicon，请先运行 scripts\sync_open_lexicons.ps1"
}

# 把用户目录与仓库目录重映射为中性路径（rustc 对多个匹配项采用最后一个）
$env:RUSTFLAGS = "--remap-path-prefix=$env:USERPROFILE=C:\build --remap-path-prefix=$RootDir=C:\src"

Push-Location $RootDir
try {
    cargo build --release -p ime-tsf-win --target-dir $TargetDir
    # 32 位 TSF 组件（WPS Office 等 32 位程序只能加载 32 位输入法 DLL）
    # 需要：rustup toolchain install stable-i686-pc-windows-gnu --profile minimal --force-non-host
    cargo +stable-i686-pc-windows-gnu build --release -p ime-tsf-win --lib --target i686-pc-windows-gnu --target-dir $TargetDir
} finally {
    Pop-Location
    Remove-Item Env:\RUSTFLAGS -ErrorAction SilentlyContinue
}

$Bin = Join-Path $TargetDir "release"
$Stage = Join-Path $RootDir "release\RustAI-Pinyin-IME-v$Version-windows-x64"
Remove-Item $Stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path (Join-Path $Stage "data\compiled") | Out-Null

Copy-Item (Join-Path $Bin "ime-global-service.exe") (Join-Path $Stage "RustAI拼音输入法_全局版.exe")
Copy-Item (Join-Path $Bin "ime-installer.exe")      (Join-Path $Stage "RustAI拼音输入法_安装程序.exe")
Copy-Item (Join-Path $Bin "ime-desktop-ui.exe")     (Join-Path $Stage "AI拼音输入法桌面体验台.exe")
Copy-Item (Join-Path $Bin "ime_tsf_win.dll")        (Join-Path $Stage "ime_tsf_win.dll")
Copy-Item (Join-Path $TargetDir "i686-pc-windows-gnu\release\ime_tsf_win.dll") (Join-Path $Stage "ime_tsf_win_x86.dll")
Copy-Item $Lexicon (Join-Path $Stage "data\compiled\merged_lexicon.bin")
Copy-Item (Join-Path $RootDir "LICENSE") $Stage
Copy-Item (Join-Path $RootDir "README.md") $Stage

# 检查产物中是否仍残留本机用户名 / 用户目录
$UserName = Split-Path -Leaf $env:USERPROFILE
foreach ($f in Get-ChildItem $Stage -File -Include *.exe, *.dll -Recurse) {
    $text = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes($f.FullName))
    if ($text.IndexOf($UserName, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
        throw "产物 $($f.Name) 中仍包含本机用户名，已中止打包"
    }
}

$Zip = "$Stage.zip"
Remove-Item $Zip -Force -ErrorAction SilentlyContinue
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::CreateFromDirectory(
    $Stage, $Zip, [System.IO.Compression.CompressionLevel]::Optimal, $true, [System.Text.Encoding]::UTF8)
Write-Host "已生成: $Zip" -ForegroundColor Green
