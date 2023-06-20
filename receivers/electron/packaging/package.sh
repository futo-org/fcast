#!/bin/sh

filename="electron-v23.0.0-$1.zip"
url="https://github.com/electron/electron/releases/download/v23.0.0/electron-v23.0.0-$1.zip"
if [ ! -f $filename ]; then
    wget $url
fi

rm fcast-receiver-$1.zip
rm -rf $1
unzip electron-v23.0.0-$1.zip -d $1
mkdir -p $1/resources
mkdir -p $1/resources/app
cp -r ../dist $1/resources/app/
cp ../package.json $1/resources/app

if [ -f "$1/electron" ]; then
    mv $1/electron $1/fcast-receiver
fi

if [ -f "$1/electron.exe" ]; then
    mv $1/electron.exe $1/fcast-receiver.exe
fi

cd $1
zip -r ../fcast-receiver-$1.zip .
cd ..
rm -rf $1