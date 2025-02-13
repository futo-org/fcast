#!/bin/bash

cd FCastReceiver
tizen build-web -- .
cd .buildResult
tizen package -t wgt -s default -- .
tizen package -t wgt -s default -r ../../FCastReceiverService/bin/Release/netcoreapp2.1/com.futo.FCastReceiverService-1.0.0.tpk -- FCastReceiver.wgt
cd ../../
