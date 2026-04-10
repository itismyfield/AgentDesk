IDENTITY := Developer ID Application: Wonchang Oh (A7LJY7HNGA)
BUNDLE_ID := com.itismyfield.agentdesk
TARGET := target/release/agentdesk

.PHONY: build clean docs-generated docs-generated-check

build:
	cargo build --release
	codesign -s "$(IDENTITY)" --options runtime --identifier "$(BUNDLE_ID)" --force "$(TARGET)"
	@codesign -v "$(TARGET)" && echo "✓ Signed: $(TARGET)"

clean:
	cargo clean

docs-generated:
	python3 scripts/generate_inventory_docs.py

docs-generated-check:
	python3 scripts/generate_inventory_docs.py --check
