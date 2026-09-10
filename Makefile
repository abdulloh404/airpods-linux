BIN_DIR := /usr/local/bin
SYSTEMD_USER_DIR := /usr/local/lib/systemd/user
APPLICATIONS_DIR := /usr/local/share/applications
UDEV_RULES_DIR := /etc/udev/rules.d
MODULES_LOAD_DIR := /etc/modules-load.d
KERNEL_RELEASE := $(shell uname -r)
KERNEL_MODULE_DIR := /lib/modules/$(KERNEL_RELEASE)/extra
PIPEWIRE_EXECUTABLE ?= $(shell command -v pipewire 2>/dev/null)
PIPEWIRE_BIN_DIR := $(patsubst %/,%,$(dir $(realpath $(PIPEWIRE_EXECUTABLE))))
PIPEWIRE_PREFIX ?= $(patsubst %/,%,$(dir $(PIPEWIRE_BIN_DIR)))
PIPEWIRE_PKG_CONFIG_PATH := $(if $(PIPEWIRE_PREFIX),$(PIPEWIRE_PREFIX)/lib/pkgconfig)
AIRPODS_PKG_CONFIG_PATH := $(PIPEWIRE_PKG_CONFIG_PATH)$(if $(and $(PIPEWIRE_PKG_CONFIG_PATH),$(PKG_CONFIG_PATH)),:)$(PKG_CONFIG_PATH)
WIREPLUMBER_PREFIX ?= /opt/pipewire-1.6.8
WIREPLUMBER_SCRIPT_DIR := $(WIREPLUMBER_PREFIX)/share/wireplumber/scripts
WIREPLUMBER_CONFIG_DIR := $(WIREPLUMBER_PREFIX)/share/wireplumber/wireplumber.conf.d

DAEMON_BINARY := target/debug/airpodsd
CLI_BINARY := target/debug/airpodsctl
GUI_BINARY := target/debug/airpods-gui
KERNEL_MODULE := kernel/airpods-power/airpods_power.ko

AIRPODS_USER := $(if $(SUDO_USER),$(SUDO_USER),$(shell id -un))
AIRPODS_USER_HOME := $(shell getent passwd "$(AIRPODS_USER)" | cut -d: -f6)
AIRPODS_USER_ID := $(shell id -u "$(AIRPODS_USER)")
AIRPODS_USER_RUNTIME_DIR := /run/user/$(AIRPODS_USER_ID)

.PHONY: all build install uninstall clean check-root check-artifacts check-user-home check-user-session

all: build

build:
	PKG_CONFIG_PATH="$(AIRPODS_PKG_CONFIG_PATH)" cargo build --workspace
	$(MAKE) -C kernel/airpods-power

check-root:
	@test "$$(id -u)" -eq 0 || { echo "Run this target with sudo."; exit 1; }

check-artifacts:
	@test -x "$(DAEMON_BINARY)" || { echo "Missing $(DAEMON_BINARY). Run 'make build' first."; exit 1; }
	@test -x "$(CLI_BINARY)" || { echo "Missing $(CLI_BINARY). Run 'make build' first."; exit 1; }
	@test -x "$(GUI_BINARY)" || { echo "Missing $(GUI_BINARY). Run 'make build' first."; exit 1; }
	@test -f "$(KERNEL_MODULE)" || { echo "Missing $(KERNEL_MODULE). Run 'make build' first."; exit 1; }

check-user-home:
	@case "$(AIRPODS_USER_HOME)" in \
		/home/*|/root) ;; \
		*) echo "Cannot safely resolve the home directory for $(AIRPODS_USER)."; exit 1 ;; \
	esac

check-user-session:
	@test -S "$(AIRPODS_USER_RUNTIME_DIR)/bus" || { echo "No active D-Bus session for $(AIRPODS_USER). Log in as that user before installing."; exit 1; }

install: check-root check-artifacts check-user-home check-user-session
	install -Dm755 "$(DAEMON_BINARY)" "$(BIN_DIR)/airpodsd"
	install -Dm755 "$(CLI_BINARY)" "$(BIN_DIR)/airpodsctl"
	install -Dm755 "$(GUI_BINARY)" "$(BIN_DIR)/airpods-gui"
	install -Dm644 systemd/airpodsd.service "$(SYSTEMD_USER_DIR)/airpodsd.service"
	install -Dm644 desktop/io.github.abdulloh404.AirPods.Gui.desktop "$(APPLICATIONS_DIR)/io.github.abdulloh404.AirPods.Gui.desktop"
	install -Dm644 wireplumber/scripts/airpods-dsp.lua "$(WIREPLUMBER_SCRIPT_DIR)/airpods-dsp.lua"
	install -Dm644 wireplumber/wireplumber.conf.d/90-airpods-dsp.conf "$(WIREPLUMBER_CONFIG_DIR)/90-airpods-dsp.conf"
	install -Dm644 "$(KERNEL_MODULE)" "$(KERNEL_MODULE_DIR)/airpods_power.ko"
	install -Dm644 udev/99-airpods-power.rules "$(UDEV_RULES_DIR)/99-airpods-power.rules"
	install -Dm644 modules-load/airpods-power.conf "$(MODULES_LOAD_DIR)/airpods-power.conf"
	depmod -a "$(KERNEL_RELEASE)"
	udevadm control --reload-rules
	systemctl --user --machine="$(AIRPODS_USER)@.host" daemon-reload
	systemctl --user --machine="$(AIRPODS_USER)@.host" enable airpodsd.service
	systemctl --user --machine="$(AIRPODS_USER)@.host" restart wireplumber.service
	systemctl --user --machine="$(AIRPODS_USER)@.host" restart airpodsd.service
	runuser -u "$(AIRPODS_USER)" -- env \
		XDG_RUNTIME_DIR="$(AIRPODS_USER_RUNTIME_DIR)" \
		DBUS_SESSION_BUS_ADDRESS="unix:path=$(AIRPODS_USER_RUNTIME_DIR)/bus" \
		"$(BIN_DIR)/airpodsctl" mic start
	@echo "Installed airpodsd, airpodsctl, airpods-gui, the WirePlumber AirPods DSP, the enabled user service, and the kernel bridge."
	@echo "Started airpodsd and enabled the virtual microphone for $(AIRPODS_USER)."
	@echo "Load now if needed: sudo modprobe airpods_power"

uninstall: check-root check-user-home
	-@systemctl --user --machine="$(AIRPODS_USER)@.host" disable --now airpodsd.service >/dev/null 2>&1
	@if [ -d /sys/module/airpods_power ]; then modprobe -r airpods_power; fi
	rm -f -- \
		"$(BIN_DIR)/airpodsd" \
		"$(BIN_DIR)/airpodsctl" \
		"$(BIN_DIR)/airpods-gui" \
		"$(SYSTEMD_USER_DIR)/airpodsd.service" \
		"$(APPLICATIONS_DIR)/io.github.abdulloh404.AirPods.Gui.desktop" \
		"$(WIREPLUMBER_SCRIPT_DIR)/airpods-dsp.lua" \
		"$(WIREPLUMBER_CONFIG_DIR)/90-airpods-dsp.conf" \
		"$(KERNEL_MODULE_DIR)/airpods_power.ko" \
		"$(UDEV_RULES_DIR)/99-airpods-power.rules" \
		"$(MODULES_LOAD_DIR)/airpods-power.conf" \
		"$(AIRPODS_USER_HOME)/.config/systemd/user/airpodsd.service" \
		"$(AIRPODS_USER_HOME)/.config/systemd/user/default.target.wants/airpodsd.service" \
		"$(AIRPODS_USER_HOME)/.local/bin/airpodsd" \
		"$(AIRPODS_USER_HOME)/.local/bin/airpodsctl" \
		"$(AIRPODS_USER_HOME)/.local/bin/airpods-gui" \
		"$(AIRPODS_USER_HOME)/.local/share/applications/io.github.abdulloh404.AirPods.Gui.desktop" \
		"$(AIRPODS_USER_HOME)/.local/state/wireplumber/airpods-linux-dsp"
	rm -rf -- \
		"$(AIRPODS_USER_HOME)/.config/airpods-linux" \
		"$(AIRPODS_USER_HOME)/.local/state/airpods-linux" \
		"$(AIRPODS_USER_HOME)/.local/share/airpods-linux" \
		"$(AIRPODS_USER_HOME)/.cache/airpods-linux"
	depmod -a "$(KERNEL_RELEASE)"
	udevadm control --reload-rules
	-@systemctl --user --machine="$(AIRPODS_USER)@.host" daemon-reload >/dev/null 2>&1
	-@systemctl --user --machine="$(AIRPODS_USER)@.host" restart wireplumber.service >/dev/null 2>&1
	@echo "Removed all installed airpods-linux files and user configuration for $(AIRPODS_USER)."

clean:
	cargo clean
	$(MAKE) -C kernel/airpods-power clean
