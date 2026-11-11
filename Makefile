PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
BIN := target/release/pbtdl

.PHONY: build install

build:
	cargo build --release

install: build
	install -d "$(DESTDIR)$(BINDIR)"
	install -m 755 "$(BIN)" "$(DESTDIR)$(BINDIR)/pbtdl"
