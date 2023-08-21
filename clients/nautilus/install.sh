#!/bin/sh

# Define the directory for the Nautilus extension
EXT_DIR="${HOME}/.local/share/nautilus-python/extensions"

# Create the directory if it doesn't exist
mkdir -p "${EXT_DIR}"

# Copy the fcast_nautilus.py to the extensions directory
cp fcast_nautilus.py "${EXT_DIR}/fcast_nautilus.py"

# Restart nautilus
nautilus -q

echo "Installation complete!"
