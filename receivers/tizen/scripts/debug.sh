#!/bin/bash
# Local development debug script

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
sdb=~/tizen-studio/tools/sdb
if [ ${machine} != "Linux" ] && [ ${machine} != "Mac" ]; then
    tizen=/c/tizen-studio/tools/ide/bin/tizen.bat
    sdb=/c/tizen-studio/tools/sdb.exe
fi

if [ "$1" ] && [ "$2" ]; then
  target=$1
  serial=$2
else
    # Hardware
    target=UN43DU7200FXZA
    serial=192.168.0.218:26101

    # Emulators
    # target=T-samsung-6.0-x86
    # target=T-samsung-9.0-x86
    # serial=emulator-26101

    # Samsung remote lab
    # target=QN55Q89RAFXKR
    # serial=127.0.0.1:52513
fi

${tizen} install -n FCastReceiver.wgt -t $target
output=$(${sdb} -s $serial shell 0 debug ql5ofothoj.fcastreceiver)
echo $output

port=$(echo $output | sed -E "s/.*port: ([0-9]+).*/\1/")
${sdb} forward tcp:${port} tcp:${port}
echo "Add 'localhost:$port' in the Chrome web inspector network target discovery list to attach to application."
