# sync_open_lexicons.ps1
# 一键拉取开源词库（雾凇拼音 rime-ice）至 data/raw，并重新编译二进制词库 data/compiled/merged_lexicon.bin
#
# 用法（在仓库根目录执行）：
#   powershell -ExecutionPolicy Bypass -File scripts\sync_open_lexicons.ps1

$ErrorActionPreference = "Stop"
$RootDir = Split-Path -Parent $PSScriptRoot
$RawDir = Join-Path $RootDir "data\raw"
New-Item -ItemType Directory -Force -Path $RawDir | Out-Null

$Sources = @(
    @{
        Name = "雾凇拼音 8105 单字表 (rime-ice)"
        Url  = "https://raw.githubusercontent.com/iDvel/rime-ice/main/cn_dicts/8105.dict.yaml"
        File = "rime_ice_8105.dict.yaml"
    },
    @{
        Name = "雾凇拼音 base 核心词库 (rime-ice)"
        Url  = "https://raw.githubusercontent.com/iDvel/rime-ice/main/cn_dicts/base.dict.yaml"
        File = "rime_ice_base.dict.yaml"
    },
    @{
        Name = "雾凇拼音英文词库 (rime-ice en_dicts)"
        Url  = "https://raw.githubusercontent.com/iDvel/rime-ice/main/en_dicts/en.dict.yaml"
        File = "melt_eng_full.dict.yaml"
    }
)

Write-Host "=== 正在同步开源拼音词库至 $RawDir ===" -ForegroundColor Cyan
foreach ($src in $Sources) {
    $outFile = Join-Path $RawDir $src.File
    Write-Host " -> 下载 $($src.Name) ..."
    try {
        Invoke-WebRequest -Uri $src.Url -OutFile $outFile -TimeoutSec 60
        Write-Host "    [成功] 已保存至 $outFile" -ForegroundColor Green
    } catch {
        Write-Warning "    [跳过] 网络连接超时或受限，将只使用 data/seed 内置种子词库: $_"
    }
}

Write-Host "`n=== 正在运行词库融合编译器 (lexicon-builder) ===" -ForegroundColor Cyan
Push-Location $RootDir
try {
    cargo run --release -p ime-lexicon --bin lexicon-builder
} finally {
    Pop-Location
}
