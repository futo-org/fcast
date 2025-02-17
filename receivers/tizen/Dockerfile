# FROM ubuntu:20.04
FROM node:23.8.0-bullseye

USER root
RUN apt update && apt install -y wget expect
RUN useradd -ms /bin/bash ubuntu
RUN usermod -a -G root ubuntu

# Tizen Studio installer requires non-root user install
USER ubuntu
WORKDIR /home/ubuntu

RUN wget https://dot.net/v1/dotnet-install.sh -O dotnet-install.sh
RUN chmod +x ./dotnet-install.sh
RUN ./dotnet-install.sh --channel 2.1
ENV DOTNET_ROOT=/home/ubuntu/.dotnet
ENV PATH=$PATH:$DOTNET_ROOT:$DOTNET_ROOT/tools

RUN wget https://download.tizen.org/sdk/Installer/tizen-studio_6.0/web-cli_Tizen_Studio_6.0_ubuntu-64.bin
RUN chmod +x web-cli_Tizen_Studio_6.0_ubuntu-64.bin
RUN yes | ./web-cli_Tizen_Studio_6.0_ubuntu-64.bin --accept-license
RUN /home/ubuntu/tizen-studio/package-manager/package-manager-cli.bin install --accept-license Baseline-SDK WebCLI TV-SAMSUNG-Public-WebAppDevelopment cert-add-on TV-SAMSUNG-Extension-Tools
ENV PATH=$PATH:/home/ubuntu/tizen-studio/tools/:/home/ubuntu/tizen-studio/tools/ide/bin/

USER root
