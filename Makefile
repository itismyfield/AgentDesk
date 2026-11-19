IDENTITY := Developer ID Application: Wonchang Oh (A7LJY7HNGA)
BUNDLE_ID := com.itismyfield.agentdesk
TARGET := target/release/agentdesk

.PHONY: build build-binary clean docs-generated docs-generated-check

build: build-binary
	codesign -s "$(IDENTITY)" --options runtime --identifier "$(BUNDLE_ID)" --force "$(TARGET)"
	@codesign -v "$(TARGET)" && echo "✓ Signed: $(TARGET)"

build-binary:
	python3 scripts/build_token.py --witness-release-source -- cargo build --release --bin agentdesk

clean:
	cargo clean

docs-generated:
	python3 scripts/generate_inventory_docs.py

docs-generated-check:
	python3 scripts/generate_inventory_docs.py --check
