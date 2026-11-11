BIN := target/release/pbtdl
CARGO_BINDIR := $(HOME)/.cargo/bin
LOCAL_BINDIR := $(HOME)/.local/bin

ifeq ($(origin BINDIR), undefined)
ifeq ($(origin PREFIX), undefined)
ifneq ($(wildcard $(CARGO_BINDIR)),)
BINDIR := $(CARGO_BINDIR)
else
BINDIR := $(LOCAL_BINDIR)
endif
else
BINDIR := $(PREFIX)/bin
endif
endif

.PHONY: build install

build:
	cargo build --release

install: build
	install -d "$(DESTDIR)$(BINDIR)"
	install -m 755 "$(BIN)" "$(DESTDIR)$(BINDIR)/pbtdl"
