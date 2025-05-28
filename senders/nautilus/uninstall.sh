#!/bin/bash

# Define the directory for the Nautilus extension
EXT_DIR="${HOME}/.local/share/nautilus-python/extensions"

# Remove the fcast_nautilus.py from the extensions directory
rm -f "${EXT_DIR}/fcast_nautilus.py"

# Restart nautilus
nautilus -q

echo "Uninstallation complete!"
