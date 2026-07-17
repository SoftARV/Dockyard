# Dockyard — build and install to a personal (per-user) prefix.
#
# No sudo: this is a one-user, one-machine app (see CLAUDE.md), so everything
# lands under ~/.local, which is already on PATH and XDG_DATA_DIRS. Override
# PREFIX for a system install (make PREFIX=/usr/local install, with sudo).

PREFIX  ?= $(HOME)/.local
BINDIR   = $(PREFIX)/bin
DATADIR  = $(PREFIX)/share
APPID    = dev.miguelrincon.Dockyard

ICON_SIZES = 16 32 48 64 128 256 512

.PHONY: all build run test check install dev-install uninstall clean

all: build

build:
	cargo build --release

run:
	cargo run

test:
	cargo test

# The bar from CLAUDE.md. --all-targets so tests are linted too.
check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

install: build dev-install
	install -Dm755 target/release/dockyard $(BINDIR)/dockyard
	@echo "Installed to $(PREFIX). Launch 'Dockyard' from the app grid, or run 'dockyard'."

# Everything except the release binary: the .desktop entry and the icons.
# Factored out so `install` isn't one long recipe. Not a way to get a dev-mode
# icon — on Wayland only the fully installed app shows one (see main.rs).
dev-install:
	install -Dm644 data/$(APPID).desktop $(DATADIR)/applications/$(APPID).desktop
	install -Dm644 data/icons/hicolor/scalable/apps/$(APPID).svg \
		$(DATADIR)/icons/hicolor/scalable/apps/$(APPID).svg
	@for sz in $(ICON_SIZES); do \
		install -Dm644 data/icons/hicolor/$${sz}x$${sz}/apps/$(APPID).png \
			$(DATADIR)/icons/hicolor/$${sz}x$${sz}/apps/$(APPID).png; \
	done
	# Refresh the caches so the icon and launcher appear without a re-login.
	# gtk-update-icon-cache needs an index.theme to build a valid cache; a
	# user-local hicolor dir usually has none, and there GNOME just scans the
	# directory instead — so only run it where it can actually succeed, rather
	# than printing a scary "cache was invalid" that doesn't matter.
	@if [ -f $(DATADIR)/icons/hicolor/index.theme ]; then \
		touch $(DATADIR)/icons/hicolor; \
		gtk-update-icon-cache -q -t -f $(DATADIR)/icons/hicolor; \
	fi
	-update-desktop-database -q $(DATADIR)/applications

uninstall:
	rm -f $(BINDIR)/dockyard
	rm -f $(DATADIR)/applications/$(APPID).desktop
	rm -f $(DATADIR)/icons/hicolor/scalable/apps/$(APPID).svg
	@for sz in $(ICON_SIZES); do \
		rm -f $(DATADIR)/icons/hicolor/$${sz}x$${sz}/apps/$(APPID).png; \
	done
	@if [ -f $(DATADIR)/icons/hicolor/index.theme ]; then \
		gtk-update-icon-cache -q -t -f $(DATADIR)/icons/hicolor; \
	fi
	-update-desktop-database -q $(DATADIR)/applications
	@echo "Uninstalled from $(PREFIX)."

clean:
	cargo clean
