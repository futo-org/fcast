@REM Local development build script

cd FCastReceiver
cmd /C tizen build-web -- .
cd .buildResult
cmd /C tizen package -t wgt -s default -- .
cmd /C tizen package -t wgt -s default -r ..\..\FCastReceiverService\bin\Release\netcoreapp2.1\com.futo.FCastReceiverService-1.0.0.tpk -- FCastReceiver.wgt
cd ../../
