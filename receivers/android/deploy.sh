#!/bin/sh
if [ -z "$ANDROID_VERSION_NAME" ] || [ -z "$ANDROID_VERSION_CODE" ]; then echo "Version name or code not specified. Skipping build."; exit 0; fi

DOCUMENT_ROOT=/var/www/html

# Build content
echo "Building content..."
./gradlew --stacktrace assembleRelease -PversionName=$ANDROID_VERSION_NAME -PversionCode=$ANDROID_VERSION_CODE
./gradlew --stacktrace bundlePlaystoreRelease -PversionName=$ANDROID_VERSION_NAME -PversionCode=$ANDROID_VERSION_CODE

# Take site offline
echo "Taking site offline..."
touch $DOCUMENT_ROOT/maintenance.file

# Swap over the content
echo "Deploying content..."
echo $ANDROID_VERSION_CODE > /var/www/html/fcast-version.txt
cp ./app/build/outputs/apk/defaultFlavor/release/app-defaultFlavor-release.apk /var/www/html/fcast-release.apk
cp ./app/build/outputs/bundle/playstoreRelease/app-playstore-release.aab /var/www/html/fcast-playstore-release.aab

# Notify Cloudflare to wipe the CDN cache
echo "Purging Cloudflare cache..."
curl -X POST "https://api.cloudflare.com/client/v4/zones/ff904f7348b9513064b23e852e328abb/purge_cache" \
     -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" \
     -H "Content-Type: application/json" \
     --data '{"purge_everything":true}'

# Take site back online
echo "Bringing site back online..."
rm $DOCUMENT_ROOT/maintenance.file
