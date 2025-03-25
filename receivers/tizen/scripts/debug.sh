#!/bin/bash

# Local development debug script
tizen install -n FCastReceiver.wgt -t T-samsung-6.5-x86
~/tizen-studio/tools/sdb -s emulator-26101 shell 0 debug ql5ofothoj.fcastreceiver
# ~/tizen-studio/tools/sdb forward tcp:34445 tcp:34445
