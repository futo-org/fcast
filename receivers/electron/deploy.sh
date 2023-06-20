#!/bin/sh
npm install
npm run build
cp package.json /var/www/html/fcastreceiver/
cp -r dist /var/www/html/fcastreceiver/
ls /var/www/html/fcastreceiver
ls /var/www/html/fcastreceiver/dist
