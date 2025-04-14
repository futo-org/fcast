#!/bin/bash

# Local development debug script
tizen install -n FCastReceiver/.buildResult/FCastReceiver.wgt -t T-samsung-5.0-x86
~/tizen-studio/tools/sdb -s emulator-26101 shell 0 debug qL5oFoTHoJ.FCastReceiver
# ~/tizen-studio/tools/sdb forward tcp:34445 tcp:34445
