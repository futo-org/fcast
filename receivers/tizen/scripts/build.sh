#!/bin/bash

# Docker container build script
npm run build
cd FCastReceiverService
dotnet build -c Release
cd ..
cd FCastReceiver
tizen build-web -- .
cd .buildResult

# Tizen OS typically uses GNOME keyring to store certificate passwords. However setting up keying
# requires dbus access and is dependent on the host envrionment. The second alternative is to put
# passwords directly in profiles.xml, but after every package it overwrites the password entries, so
# it has to be regenerated on every packaging...
# https://stackoverflow.com/a/61718469

tizen security-profiles add --active --force --name default --author $CERT_PATH/author/default/author.p12 --password $CERT_AUTHOR_PASSWORD --dist $CERT_PATH/SamsungCertificate/default/distributor.p12 --dist-password $CERT_DIST_PASSWORD
tizen cli-config "profiles.path=/home/ubuntu/tizen-studio-data/profile/profiles.xml"
sed -i "s|$CERT_PATH/author/default/author.pwd|$CERT_AUTHOR_PASSWORD|g" /home/ubuntu/tizen-studio-data/profile/profiles.xml
sed -i "s|$CERT_PATH/SamsungCertificate/default/distributor.pwd|$CERT_DIST_PASSWORD|g" /home/ubuntu/tizen-studio-data/profile/profiles.xml
../../scripts/package.sh tizen package -t wgt -s default -- .

tizen security-profiles add --active --force --name default --author $CERT_PATH/author/default/author.p12 --password $CERT_AUTHOR_PASSWORD --dist $CERT_PATH/SamsungCertificate/default/distributor.p12 --dist-password $CERT_DIST_PASSWORD
tizen cli-config "profiles.path=/home/ubuntu/tizen-studio-data/profile/profiles.xml"
sed -i "s|$CERT_PATH/author/default/author.pwd|$CERT_AUTHOR_PASSWORD|g" /home/ubuntu/tizen-studio-data/profile/profiles.xml
sed -i "s|$CERT_PATH/SamsungCertificate/default/distributor.pwd|$CERT_DIST_PASSWORD|g" /home/ubuntu/tizen-studio-data/profile/profiles.xml
../../scripts/package.sh tizen package -t wgt -s default -r ../../FCastReceiverService/bin/Release/netcoreapp2.1/com.futo.FCastReceiverService-1.0.0.tpk -- FCastReceiver.wgt

cd ../../
