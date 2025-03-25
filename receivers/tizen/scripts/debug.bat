@REM Local development debug script
cmd /C tizen install -n FCastReceiver.wgt -t UN43DU7200FXZA -- FCastReceiver/.buildResult
cmd /C C:\tizen-studio\tools\sdb.exe -s 192.168.0.218:26101 shell 0 debug ql5ofothoj.fcastreceiver

@REM Emulators
@REM cmd /C tizen install -n FCastReceiver/.buildResult/FCastReceiver.wgt -t T-samsung-9.0-x86
@REM cmd /C C:\tizen-studio\tools\sdb.exe -s emulator-26101 shell 0 debug ql5ofothoj.fcastreceiver

@REM Samsung remote lab
@REM cmd /C tizen install -n FCastReceiver.wgt -t QN55Q89RAFXKR -- FCastReceiver/.buildResult
@REM cmd /C C:\tizen-studio\tools\sdb.exe -s 127.0.0.1:52513 shell 0 debug ql5ofothoj.fcastreceiver
@REM C:\tizen-studio\tools\sdb.exe forward tcp:34445 tcp:34445

