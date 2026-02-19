Name:           fcast-receiver
Version:        0.1.0
Release:        1%{?dist}
Summary:        FCast Receiver for displaying multimedia content
License:        GPLv3
URL:            https://fcast.org/

BuildRequires:  clang-devel
BuildRequires:  rust
BuildRequires:  gstreamer1-devel
BuildRequires:  gstreamer1-plugins-base-devel
BuildRequires:  gstreamer1-plugins-good
BuildRequires:  gstreamer1-plugins-bad-free-devel
BuildRequires:  dav1d-devel

Requires:       gstreamer1
Requires:       gstreamer1-plugins-base
Requires:       gstreamer1-plugins-good
Requires:       gstreamer1-plugins-bad-free
Requires:       gstreamer1-libav

%description
FCast is an open source protocol that enables streaming of multimedia content
between devices, supporting various stream types such as DASH, HLS, and mp4.
This package provides the desktop receiver application.

%prep
# Source is provided by CI, no prep needed

%build
cargo build --release --package desktop-receiver

%install
install -Dm755 target/release/desktop-receiver %{buildroot}%{_bindir}/fcast-receiver
install -Dm644 .flatpak/org.fcast.Receiver.desktop %{buildroot}%{_datadir}/applications/org.fcast.Receiver.desktop
install -Dm644 .flatpak/org.fcast.Receiver.metainfo.xml %{buildroot}%{_metainfodir}/org.fcast.Receiver.metainfo.xml

%files
%{_bindir}/fcast-receiver
%{_datadir}/applications/org.fcast.Receiver.desktop
%{_metainfodir}/org.fcast.Receiver.metainfo.xml

%changelog
Initial release
