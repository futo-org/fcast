FROM node:22.10.0-bookworm

RUN dpkg --add-architecture i386
RUN apt update && apt install -y zip dpkg fakeroot rpm wget p7zip-full unzip

RUN mkdir -pm755 /etc/apt/keyrings
RUN wget -O /etc/apt/keyrings/winehq-archive.key https://dl.winehq.org/wine-builds/winehq.key
RUN wget -NP /etc/apt/sources.list.d/ https://dl.winehq.org/wine-builds/debian/dists/bookworm/winehq-bookworm.sources

RUN apt update && apt install -y --install-recommends winehq-staging mono-devel

RUN wget https://github.com/PietJankbal/Chocolatey-for-wine/releases/download/v0.5a.745/Chocolatey-for-wine.7z
RUN 7za x Chocolatey-for-wine.7z
RUN wine Chocolatey-for-wine/ChoCinstaller_0.5a.745.exe

# Issues building in docker...
# RUN wine choco install wixtoolset  --version=3.14.1
# ENV WINEPATH="/root/.wine/drive_c/Program Files (x86)/WiX Toolset v3.14/bin/"

RUN wget https://github.com/wixtoolset/wix3/releases/download/wix3141rtm/wix314-binaries.zip
RUN unzip wix314-binaries.zip -d wix314-binaries
ENV WINEPATH="/wix314-binaries/"
