FROM node:16.20.2-bullseye

ARG TV_IP
ARG PASSPHRASE

RUN apt update && apt install -y expect jq

RUN npm install -g @webos-tools/cli
RUN ares-setup-device --add tv --info "host=${TV_IP}" --info "default=true"

COPY scripts/get_private_key.sh .
RUN ./get_private_key.sh ares-novacom --device tv --getkey
