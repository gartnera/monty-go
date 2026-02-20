.PHONY: build test clean

WASM_TARGET = wasm32-wasip1
WASM_BINARY = monty.wasm

# Build the WASM binary from the Rust shim.
build:
	cargo build --target $(WASM_TARGET) -p monty-wasm --release
	cp target/$(WASM_TARGET)/release/monty_wasm.wasm $(WASM_BINARY)

# Run Go tests.
test: build
	go test -v -count=1 ./...

# Remove build artifacts.
clean:
	cargo clean
	rm -f $(WASM_BINARY)
