PREFIX ?= $(HOME)/.local
BINDIR  = $(PREFIX)/bin

.PHONY: all install uninstall clean

all:
	cargo build --release

install: all
	install -Dm755 target/release/oc $(DESTDIR)$(BINDIR)/oc

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/oc

clean:
	cargo clean
