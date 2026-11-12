# mpv only loads C plugins with a .so extension on macOS/Linux (.dll on Windows),
# so copy cargo's libmpvlink.dylib/.so output to mpvlink.so.
#
#   make build                 # release build -> target/release/mpvlink.so
#   make build PROFILE=debug   # debug build   -> target/debug/mpvlink.so
#   make signal                # run the signaling server on :8787
#   make send ROOM=ABCD12      # stream a test pattern into room ABCD12
#   make run                   # mpv with the plugin; join the room from the menu
#   make linux                 # cross-compile everything for x86_64 Linux -> dist/linux-x86_64/
#   make linux PKG=mpvlink-signal   # ...or just one package
#   make windows               # cross-compile everything for x86_64 Windows -> dist/windows-x86_64/
#   make windows PKG=mpvlink   # ...or just one package

PROFILE     ?= release
CARGO_FLAGS := $(if $(filter release,$(PROFILE)),--release,)
LIB_EXT     := $(if $(filter Darwin,$(shell uname)),dylib,so)
PLUGIN      := target/$(PROFILE)/mpvlink.so
SCRIPTS_DIR := $(HOME)/.config/mpv/scripts

# ---- Linux builds -----------------------------------------------------------
# On macOS this cross-compiles with a GCC cross toolchain whose binaries are
# named after the target triple (x86_64-unknown-linux-gnu-gcc etc.):
#   brew install SergioBenitez/osxct/x86_64-unknown-linux-gnu
#   rustup target add x86_64-unknown-linux-gnu
# Its old glibc (2.26 sysroot) means the binaries run on any current distro.
# On Linux it's a plain native build. Override LINUX_TARGET / LINUX_TOOLCHAIN
# for another triple or toolchain prefix.
LINUX_TARGET    ?= x86_64-unknown-linux-gnu
LINUX_TOOLCHAIN ?= $(LINUX_TARGET)
LINUX_ARCH      := $(firstword $(subst -, ,$(LINUX_TARGET)))
LINUX_DIST      := dist/linux-$(LINUX_ARCH)
LINUX_OUT       := target/$(LINUX_TARGET)/release
LINUX_PKG_FLAGS := $(if $(PKG),-p $(PKG),--workspace)
# What to copy into dist/ (only the requested package's outputs, not stale ones).
LINUX_BINS      := $(if $(PKG),$(filter $(PKG),mpvlink-signal mpvlink-send),mpvlink-signal mpvlink-send)
LINUX_PLUGIN    := $(if $(PKG),$(filter mpvlink,$(PKG)),mpvlink)
TARGET_ENV      := $(subst -,_,$(LINUX_TARGET))
TARGET_ENV_UC   := $(shell echo $(TARGET_ENV) | tr a-z A-Z)
ifeq ($(shell uname),Darwin)
LINUX_ENV := CARGO_TARGET_$(TARGET_ENV_UC)_LINKER=$(LINUX_TOOLCHAIN)-gcc \
             CC_$(TARGET_ENV)=$(LINUX_TOOLCHAIN)-gcc \
             CXX_$(TARGET_ENV)=$(LINUX_TOOLCHAIN)-g++ \
             AR_$(TARGET_ENV)=$(LINUX_TOOLCHAIN)-ar
LINUX_CHECK := command -v $(LINUX_TOOLCHAIN)-gcc >/dev/null || \
               { echo "missing $(LINUX_TOOLCHAIN)-gcc; install with:"; \
                 echo "  brew install SergioBenitez/osxct/$(LINUX_TARGET)"; exit 1; }
else
LINUX_CHECK := true
endif

# ---- Windows builds ---------------------------------------------------------
# Cross-compiles with MinGW-w64 (the windows-gnu target):
#   brew install mingw-w64        (Debian/Ubuntu: apt install mingw-w64)
#   rustup target add x86_64-pc-windows-gnu
# The outputs need only DLLs that ship with Windows 10+: the C/C++ runtimes are
# linked statically (OpenH264, used by mpvlink-send, is C++).
WINDOWS_TARGET    ?= x86_64-pc-windows-gnu
WINDOWS_TOOLCHAIN ?= x86_64-w64-mingw32
WINDOWS_DIST      := dist/windows-$(firstword $(subst -, ,$(WINDOWS_TARGET)))
WINDOWS_OUT       := target/$(WINDOWS_TARGET)/release
WINDOWS_PKG_FLAGS := $(if $(PKG),-p $(PKG),--workspace)
WINDOWS_BINS      := $(if $(PKG),$(filter $(PKG),mpvlink-signal mpvlink-send),mpvlink-signal mpvlink-send)
WINDOWS_PLUGIN    := $(if $(PKG),$(filter mpvlink,$(PKG)),mpvlink)
WIN_ENV           := $(subst -,_,$(WINDOWS_TARGET))
WIN_ENV_UC        := $(shell echo $(WIN_ENV) | tr a-z A-Z)
# The cc crate links libstdc++ dynamically by default; CXXSTDLIB makes it
# static, which needs the toolchain's lib directory on the search path.
WINDOWS_ENV = CARGO_TARGET_$(WIN_ENV_UC)_LINKER=$(WINDOWS_TOOLCHAIN)-gcc \
              CARGO_TARGET_$(WIN_ENV_UC)_RUSTFLAGS="-C target-feature=+crt-static -L native=$$(dirname "$$($(WINDOWS_TOOLCHAIN)-g++ -print-file-name=libstdc++.a)")" \
              CC_$(WIN_ENV)=$(WINDOWS_TOOLCHAIN)-gcc \
              CXX_$(WIN_ENV)=$(WINDOWS_TOOLCHAIN)-g++ \
              AR_$(WIN_ENV)=$(WINDOWS_TOOLCHAIN)-ar \
              CXXSTDLIB_$(WIN_ENV)=static=stdc++

.PHONY: build install run signal send test clean linux windows

# Copy to a temp file and rename instead of overwriting in place: macOS kills
# any process that loads a code-signed file modified in place, and a running
# mpv (e.g. a debug session) may still have the old plugin mapped.
build:
	cargo build -p mpvlink $(CARGO_FLAGS)
	cp target/$(PROFILE)/libmpvlink.$(LIB_EXT) $(PLUGIN).tmp
	mv -f $(PLUGIN).tmp $(PLUGIN)

install: build
	mkdir -p $(SCRIPTS_DIR)
	cp $(PLUGIN) $(SCRIPTS_DIR)/mpvlink.so.tmp
	mv -f $(SCRIPTS_DIR)/mpvlink.so.tmp $(SCRIPTS_DIR)/mpvlink.so

# Start mpv with nothing loaded; the plugin opens the join menu.
run: build
	mpv --idle=yes --script=$(PLUGIN)

signal:
	cargo run -p mpvlink-signal $(CARGO_FLAGS)

send:
	@test -n "$(ROOM)" || (echo "usage: make send ROOM=CODE" && exit 1)
	cargo run -p mpvlink-send $(CARGO_FLAGS) -- --room $(ROOM)

test:
	cargo test --workspace

linux:
	@$(LINUX_CHECK)
	@rustup target list --installed 2>/dev/null | grep -qx $(LINUX_TARGET) || \
		{ echo "missing Rust target; install with: rustup target add $(LINUX_TARGET)"; exit 1; }
	$(LINUX_ENV) cargo build --release --target $(LINUX_TARGET) $(LINUX_PKG_FLAGS)
	mkdir -p $(LINUX_DIST)
	$(if $(LINUX_BINS),cp $(addprefix $(LINUX_OUT)/,$(LINUX_BINS)) $(LINUX_DIST)/)
	$(if $(LINUX_PLUGIN),cp $(LINUX_OUT)/libmpvlink.so $(LINUX_DIST)/mpvlink.so)
	@echo "Linux build in $(LINUX_DIST)/:"; ls -lh $(LINUX_DIST) | tail -n +2

windows:
	@command -v $(WINDOWS_TOOLCHAIN)-gcc >/dev/null || \
		{ echo "missing $(WINDOWS_TOOLCHAIN)-gcc; install MinGW-w64 (brew install mingw-w64)"; exit 1; }
	@rustup target list --installed 2>/dev/null | grep -qx $(WINDOWS_TARGET) || \
		{ echo "missing Rust target; install with: rustup target add $(WINDOWS_TARGET)"; exit 1; }
	$(WINDOWS_ENV) cargo build --release --target $(WINDOWS_TARGET) $(WINDOWS_PKG_FLAGS)
	mkdir -p $(WINDOWS_DIST)
	$(if $(WINDOWS_BINS),cp $(addsuffix .exe,$(addprefix $(WINDOWS_OUT)/,$(WINDOWS_BINS))) $(WINDOWS_DIST)/)
	$(if $(WINDOWS_PLUGIN),cp $(WINDOWS_OUT)/mpvlink.dll $(WINDOWS_DIST)/)
	@echo "Windows build in $(WINDOWS_DIST)/:"; ls -lh $(WINDOWS_DIST) | tail -n +2

clean:
	cargo clean
	rm -rf dist
