FROM node:22.10.0-bookworm

RUN dpkg --add-architecture i386
RUN apt update && apt install -y zip dpkg fakeroot rpm wget p7zip-full unzip

RUN mkdir -pm755 /etc/apt/keyrings
RUN wget -O /etc/apt/keyrings/winehq-archive.key https://dl.winehq.org/wine-builds/winehq.key
RUN wget -NP /etc/apt/sources.list.d/ https://dl.winehq.org/wine-builds/debian/dists/bookworm/winehq-bookworm.sources
RUN echo "deb http://deb.debian.org/debian bookworm main contrib non-free" > /etc/apt/sources.list
RUN echo "deb http://deb.debian.org/debian bookworm-updates main contrib non-free" >> /etc/apt/sources.list

RUN apt update && apt install -y --install-recommends winehq-stable mono-devel winetricks
RUN winetricks -q -f dotnet45

RUN wget https://github.com/wixtoolset/wix3/releases/download/wix3141rtm/wix314-binaries.zip
RUN unzip wix314-binaries.zip -d wix314-binaries
ENV WINEPATH="/wix314-binaries/"
