# mpv only loads C plugins with a .so extension on macOS/Linux (.dll on Windows),
# so copy cargo's libmpvlink.dylib/.so output to mpvlink.so.
#
#   make build                 # release build -> target/release/mpvlink.so
#   make build PROFILE=debug   # debug build   -> target/debug/mpvlink.so
#   make signal                # run the signaling server on :8787
#   make send ROOM=ABCD12      # stream a test pattern into room ABCD12
#   make run                   # mpv with the plugin; join the room from the menu

PROFILE     ?= release
CARGO_FLAGS := $(if $(filter release,$(PROFILE)),--release,)
LIB_EXT     := $(if $(filter Darwin,$(shell uname)),dylib,so)
PLUGIN      := target/$(PROFILE)/mpvlink.so
SCRIPTS_DIR := $(HOME)/.config/mpv/scripts

.PHONY: build install run signal send test clean

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

clean:
	cargo clean
