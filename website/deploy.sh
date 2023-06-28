#!/bin/sh
DOCUMENT_ROOT=/var/www/fcast

# Take site offline
echo "Taking site offline..."
touch $DOCUMENT_ROOT/maintenance.file

# Swap over the content
echo "Deploying content..."
cp index.html $DOCUMENT_ROOT/
cp privacy-policy.html $DOCUMENT_ROOT/
cp favicon.png $DOCUMENT_ROOT/
cp -r css $DOCUMENT_ROOT/
cp -r images $DOCUMENT_ROOT/
cp -r js $DOCUMENT_ROOT/
cp -r vendor $DOCUMENT_ROOT/

# Notify Cloudflare to wipe the CDN cache
echo "Purging Cloudflare cache..."
curl -X POST "https://api.cloudflare.com/client/v4/zones/018ad3b2bbb3bca14f2dd793ddb78de1/purge_cache" \
     -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" \
     -H "Content-Type: application/json" \
     --data '{"purge_everything":true}'

sleep 30

# Take site back online
echo "Bringing site back online..."
rm $DOCUMENT_ROOT/maintenance.file
