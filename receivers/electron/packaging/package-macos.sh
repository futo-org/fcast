#!/bin/sh

filename="electron-v23.0.0-$1.zip"
url="https://github.com/electron/electron/releases/download/v23.0.0/electron-v23.0.0-$1.zip"
if [ ! -f $filename ]; then
  wget $url
fi

rm fcast-receiver-$1.zip
rm -rf $1
unzip electron-v23.0.0-$1.zip -d $1
mkdir -p $1/Electron.app/Contents/Resources/app
mkdir -p $1/Electron.app/Contents/Resources/app
cp -r ../dist $1/Electron.app/Contents/Resources/app/
cp ../package.json $1/Electron.app/Contents/Resources/app
mv $1/Electron.app $1/FCastReceiver.app
cd $1
zip -r ../fcast-receiver-$1.zip FCastReceiver.app
cd ..
rm -rf $1