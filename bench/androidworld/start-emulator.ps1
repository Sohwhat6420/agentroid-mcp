# The detached supervisor starts this with an empty environment, so everything is spelt out.
$home_ = 'C:\Users\USER'
$sdk = 'C:\Users\USER\AndroidSdk'
$exe = "$sdk\emulator\emulator.exe"
$pidFile = "$sdk\emulator.pid"
$env:USERPROFILE = $home_
$env:HOMEDRIVE = 'C:'
$env:HOMEPATH = '\Users\USER'
$env:LOCALAPPDATA = "$home_\AppData\Local"
$env:APPDATA = "$home_\AppData\Roaming"
$env:TEMP = "$home_\AppData\Local\Temp"
$env:TMP = $env:TEMP
$env:ANDROID_SDK_ROOT = $sdk
$env:ANDROID_HOME = $sdk
$env:ANDROID_AVD_HOME = "$home_\.android\avd"
# results\avd.name, when present, names a different AVD (the small-RAM fallback); the default is the standard one.
$avdName = 'AndroidWorldAvd'
$avdFile = Join-Path $PSScriptRoot 'results\avd.name'
if (Test-Path $avdFile) { $avdName = (Get-Content $avdFile -Raw).Trim() }
$emuArgs = @('-avd',$avdName,'-no-window','-no-audio','-no-boot-anim','-no-snapshot','-no-metrics','-gpu','swiftshader_indirect','-port','5554','-grpc','8554')
$p = Start-Process -FilePath $exe -ArgumentList $emuArgs -WindowStyle Hidden -PassThru -WorkingDirectory "$sdk\emulator"
Set-Content -Path $pidFile -Value $p.Id
Wait-Process -Id $p.Id
