IDENTITY ?= Developer ID Application: Wonchang Oh (A7LJY7HNGA)
BUNDLE_ID ?= com.itismyfield.agentdesk
TARGET ?= target/release/agentdesk

.PHONY: build clean docs-generated docs-generated-check

build:
	cargo build --release
	@identity="$(IDENTITY)"; \
	if [ -n "$$identity" ] && [ "$$identity" != "-" ] && command -v security >/dev/null 2>&1; then \
		if ! security find-identity -v -p codesigning 2>/dev/null | grep -Fq "$$identity"; then \
			echo "⚠ Signing identity '$$identity' not found; falling back to ad-hoc signature"; \
			identity="-"; \
		fi; \
	elif [ -z "$$identity" ]; then \
		identity="-"; \
	fi; \
	if [ "$$identity" = "-" ]; then \
		codesign -s "$$identity" --identifier "$(BUNDLE_ID)" --force "$(TARGET)"; \
	else \
		codesign -s "$$identity" --options runtime --identifier "$(BUNDLE_ID)" --force "$(TARGET)"; \
	fi; \
	codesign -v "$(TARGET)" && echo "✓ Signed: $(TARGET) [$$identity]"

clean:
	cargo clean

docs-generated:
	python3 scripts/generate_inventory_docs.py

docs-generated-check:
	python3 scripts/generate_inventory_docs.py --check
