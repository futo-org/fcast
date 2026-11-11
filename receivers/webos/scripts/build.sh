#!/bin/bash

cd fcast-receiver
npm run build
cd ../fcast-receiver-service
npm run build
cd ../

ares-package fcast-receiver/dist/ fcast-receiver-service/dist/ --no-minify
