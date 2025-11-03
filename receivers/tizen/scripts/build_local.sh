#!/bin/bash
# Local development build script

unameOut="$(uname -s)"
case "${unameOut}" in
    Linux*)     machine=Linux;;
    Darwin*)    machine=Mac;;
    CYGWIN*)    machine=Cygwin;;
    MINGW*)     machine=MinGw;;
    MSYS_NT*)   machine=MSys;;
    *)          machine="UNKNOWN:${unameOut}"
esac

tizen=tizen
if [ ${machine} != "Linux" ] && [ ${machine} != "Mac" ]; then
    tizen=/c/tizen-studio/tools/ide/bin/tizen.bat
fi

npm run build
cd FCastReceiverService
dotnet build -c Release
cd ..
cd FCastReceiver
${tizen} build-web -- .
cd .buildResult
${tizen} package -t wgt -s default -- .
${tizen} package -t wgt -s default -r ../../FCastReceiverService/bin/Release/tizen60/com.futo.fcastreceiverservice-1.0.0.tpk -- "FCast Receiver.wgt"
mv "FCast Receiver.wgt" ../../FCastReceiver.wgt
cd ../../
