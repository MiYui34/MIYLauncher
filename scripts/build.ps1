<#
  构建分发包：dist\MIYLauncher.exe + dist\plugins\<plugin-id>\{manifest.json, *.dll}

  核心插件 miy.mc（版本清单 / 下载 / 启动）已经编进 exe（crates\miy-host\src\embedded.rs），
  所以默认不再打包它：用户只拿一个 MIYLauncher.exe 就能装并启动游戏。
  这里剩下的 hello / boom 是插件系统的演示与 `--spike` 自检用的。

  用法（Windows PowerShell 5.1 与 pwsh 7 都能跑）：
    powershell -File scripts\build.ps1            # 只构建并组装
    powershell -File scripts\build.ps1 -Verify    # 构建 + dumpbin 依赖表检查 + 跑 --spike 自检
#>
param(
    [switch]$Verify
)

$ErrorActionPreference = 'Continue'
# PS 7.3+ 才有的开关，5.1 下要跳过（否则 set 不存在的变量只是普通赋值，无害但不直观）
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root 'dist'
$target = Join-Path $root 'target'
$dumpbin = 'C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Tools\MSVC\14.51.36231\bin\Hostx64\x64\dumpbin.exe'

# 编进 exe 的插件：宿主以 rlib 静态链接，dist 里再放一份 dll 只会误导用户。
$builtin = @('miy-mc')
# 单文件启动器的体积上限。宿主 release 约 0.8 MB，miy.mc 全量约 0.6 MB，
# 4.5 MB 是用户能接受的"几秒钟下载完"的量级，超了就说明有人往本体里塞了大资源。
$exeBudget = 4.5MB

function Invoke-Cargo {
    param([string[]]$CargoArgs)
    & cargo @CargoArgs 2>&1 | ForEach-Object { if ($_ -notmatch '^\s*$') { Write-Host "  $_" } }
    if ($LASTEXITCODE -ne 0) { throw "cargo $($CargoArgs -join ' ') 失败（exit $LASTEXITCODE）" }
}

Push-Location $root
try {
    Write-Host '== 构建宿主 (release) =='
    Invoke-Cargo @('build', '-p', 'miy-host', '--release')

    # 在 plugins\ 下找所有带 manifest.json 的 crate（目录名即 cargo 包名）
    $pluginCrates = @(Get-ChildItem (Join-Path $root 'plugins') -Directory |
        Where-Object { Test-Path (Join-Path $_.FullName 'manifest.json') })
    # 内置插件由宿主一起编译（cargo build -p miy-host 已经把它连进去了），
    # 不必再单独编 cdylib，也不必拷进 dist。
    $diskCrates = @($pluginCrates | Where-Object { $builtin -notcontains $_.Name })

    if ($diskCrates.Count -gt 0) {
        Write-Host "== 构建插件 (profile=plugin): $($diskCrates.Name -join ', ') =="
        $pkgs = @('build')
        foreach ($c in $diskCrates) { $pkgs += @('-p', $c.Name) }
        $pkgs += @('--profile', 'plugin')
        Invoke-Cargo $pkgs
    }

    Write-Host "== 组装 $dist =="
    # 只重建启动器本体与插件；dist\MC 是玩家数据，构建不能碰
    if (-not (Test-Path $dist)) { New-Item -ItemType Directory $dist -Force | Out-Null }
    Remove-Item (Join-Path $dist 'plugins') -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item (Join-Path $dist 'MIYLauncher.exe') -Force -ErrorAction SilentlyContinue
    Copy-Item (Join-Path $target 'release\MIYLauncher.exe') $dist

    foreach ($c in $diskCrates) {
        # 必须显式按 UTF-8 读：PS 5.1 的 Get-Content -Raw 按 ANSI(GBK) 解码，
        # UTF-8 中文的尾字节里会出现 0x5C（反斜杠），直接把 JSON 读成非法转义。
        $json = [System.IO.File]::ReadAllText((Join-Path $c.FullName 'manifest.json'),
            [System.Text.Encoding]::UTF8)
        $mf = $json | ConvertFrom-Json
        $out = Join-Path $dist "plugins\$($mf.id)"
        New-Item -ItemType Directory $out -Force | Out-Null
        Copy-Item (Join-Path $c.FullName 'manifest.json') $out
        $dll = Join-Path $target "plugin\$($mf.entry)"
        if (-not (Test-Path $dll)) { throw "找不到插件产物: $dll" }
        Copy-Item $dll $out
        # 插件自带的界面文件：宿主只按 manifest 里的入口去读，不再打包进 exe。
        $uiSrc = Join-Path $c.FullName 'ui'
        if (Test-Path $uiSrc) {
            Copy-Item $uiSrc (Join-Path $out 'ui') -Recurse -Force
        }
        Write-Host ("  plugins\{0}\{1}  ({2} B)" -f $mf.id, $mf.entry, (Get-Item $dll).Length)
    }

    $exe = Join-Path $dist 'MIYLauncher.exe'
    $exeLen = (Get-Item $exe).Length
    Write-Host ''
    Write-Host ('  MIYLauncher.exe  {0:N0} B  （内置 miy.mc，单文件即可启动游戏）' -f $exeLen)
    $total = (Get-ChildItem $dist -Recurse -File | Measure-Object Length -Sum).Sum
    Write-Host ('  分发包合计       {0:N0} B' -f $total)
    if ($exeLen -gt $exeBudget) {
        Write-Host ('  [FAIL] 本体超过体积预算 {0:N1} MB（当前 {1:N2} MB）' -f ($exeBudget / 1MB), ($exeLen / 1MB)) -ForegroundColor Red
        $code = 1
    } else {
        Write-Host ('  [OK] 本体 {0:N2} MB，预算 {1:N1} MB' -f ($exeLen / 1MB), ($exeBudget / 1MB)) -ForegroundColor Green
    }

    if ($Verify) {
        if (-not (Test-Path $dumpbin)) { throw "找不到 dumpbin: $dumpbin" }
        Write-Host ''
        Write-Host '== 依赖表检查（动静 CRT / WebView2Loader 必须是静态的）=='
        $deps = & $dumpbin /nologo /dependents $exe 2>&1 | Out-String
        $banned = @('VCRUNTIME140', 'VCRUNTIME140_1', 'MSVCP140', 'WebView2Loader', 'api-ms-win-crt')
        $bad = @()
        foreach ($b in $banned) { if ($deps -match [regex]::Escape($b)) { $bad += $b } }
        $dllLines = ($deps -split "`n") | Where-Object { $_ -match '\.dll' } | ForEach-Object { $_.Trim() }
        Write-Host '  依赖：'
        $dllLines | ForEach-Object { Write-Host "    $_" }
        if ($bad.Count -gt 0) {
            Write-Host "  [FAIL] 出现了不该有的依赖: $($bad -join ', ')" -ForegroundColor Red
            exit 1
        }
        Write-Host '  [OK] 无 VCRUNTIME / MSVCP / WebView2Loader / api-ms-win-crt 依赖'

        Write-Host ''
        Write-Host '== --spike 自检 =='
        # 自检会故意触发 panic 与访问冲突，这些走 stderr；PS 5.1 会把原生 stderr
        # 当错误记录，打出一堆 NativeCommandError 吓人。收成文本再输出，日志才干净。
        #
        # 注意：发行版是 GUI 子系统（双击不弹控制台），PowerShell 的 `&` 不会等它，
        # 必须用 Start-Process -Wait 并把 stdout/stderr 重定向到文件才能拿到结果。
        $so = [System.IO.Path]::GetTempFileName()
        $se = [System.IO.Path]::GetTempFileName()
        $p = Start-Process -FilePath $exe -ArgumentList '--spike' -NoNewWindow -Wait -PassThru `
            -RedirectStandardOutput $so -RedirectStandardError $se
        Get-Content $so -Encoding UTF8 | ForEach-Object { Write-Host "$_" }
        Get-Content $se -Encoding UTF8 | ForEach-Object { Write-Host "$_" }
        Remove-Item $so, $se -ErrorAction SilentlyContinue
        $code = $p.ExitCode
        Write-Host ''
        if ($code -eq 0) { Write-Host '  [OK] spike 自检全通过' -ForegroundColor Green }
        else { Write-Host "  [FAIL] spike 自检返回 $code" -ForegroundColor Red }

        Write-Host ''
        # 窗口按钮验收脚本属于本地测试工具，仓库里没有；有就跑，没有就跳过。
        $winScript = Join-Path $PSScriptRoot 'verify-window.ps1'
        if (Test-Path $winScript) {
            Write-Host '== 窗口按钮验收（无边框窗口的自绘按钮）=='
            & $winScript -Exe $exe 2>&1 | ForEach-Object { Write-Host "$_" }
            $winCode = $LASTEXITCODE
            if ($winCode -ne 0) { $code = $winCode }
        } else {
            Write-Host '  [skip] 没有 verify-window.ps1，跳过窗口按钮验收' -ForegroundColor Yellow
        }

        Write-Host ''
        if ($code -eq 0) { Write-Host '  [OK] 全部验收通过' -ForegroundColor Green }
        exit $code
    }
}
finally {
    Pop-Location
}
