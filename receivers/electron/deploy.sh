#!/bin/sh
DOCUMENT_ROOT=/var/www/html

# Build content
echo "Building content..."
npm install
cd packaging
sh package-all.sh

# Take site offline
echo "Taking site offline..."
touch $DOCUMENT_ROOT/maintenance.file

# Swap over the content
echo "Deploying content..."
cp fcast-receiver-*.zip $DOCUMENT_ROOT/fcastreceiver/
cd ..
cp package.json $DOCUMENT_ROOT/fcastreceiver/
cp -r dist $DOCUMENT_ROOT/fcastreceiver/

# Notify Cloudflare to wipe the CDN cache
echo "Purging Cloudflare cache..."
curl -X POST "https://api.cloudflare.com/client/v4/zones/$CLOUDFLARE_ZONE_ID/purge_cache" \
     -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" \
     -H "Content-Type: application/json" \
     --data '{"purge_everything":true}'

# Take site back online
echo "Bringing site back online..."
rm $DOCUMENT_ROOT/maintenance.file