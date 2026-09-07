.PHONY: build clean

build:
	cargo build --workspace
	$(MAKE) -C kernel/airpods-power

clean:
	cargo clean
	$(MAKE) -C kernel/airpods-power clean

