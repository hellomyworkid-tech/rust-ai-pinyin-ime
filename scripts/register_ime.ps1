# register_ime.ps1
# 编译 ime_tsf_win.dll，并提示如何将其注册为 Windows TSF 文本输入服务（注册需管理员权限）。
# 一般用户直接运行「RustAI拼音输入法_安装程序.exe」即可，无需使用本脚本。

param(
    [switch]$Unregister
)

$ErrorActionPreference = "Stop"
$RootDir = Split-Path -Parent $PSScriptRoot
$Clsid = "{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}"

Push-Location $RootDir
try {
    $TargetDir = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory
    $DllPath = Join-Path $TargetDir "release\ime_tsf_win.dll"

    if ($Unregister) {
        Write-Host "请以管理员身份运行: regsvr32.exe /u `"$DllPath`"" -ForegroundColor Yellow
        Write-Host "TSF 组件 CLSID: $Clsid"
        exit 0
    }

    if (-not (Test-Path $DllPath)) {
        Write-Host "正在编译 Release 版 ime_tsf_win.dll ..." -ForegroundColor Cyan
        cargo build --release -p ime-tsf-win
    }
} finally {
    Pop-Location
}

Write-Host "编译完成: $DllPath" -ForegroundColor Green
Write-Host "如需注册至系统语言栏，请以管理员身份运行: regsvr32.exe `"$DllPath`"" -ForegroundColor Cyan
