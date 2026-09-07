// SPDX-License-Identifier: GPL-2.0-only
//! สร้าง power_supply เสมือนสำหรับแบตเตอรี่ AirPods ข้างซ้ายและข้างขวา

#include <linux/fs.h>
#include <linux/err.h>
#include <linux/jiffies.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/power_supply.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/workqueue.h>

#define AIRPODS_POWER_PROTOCOL_VERSION 1
#define AIRPODS_POWER_STALE_TIMEOUT_MS 90000

struct airpods_power_update {
	__u8 version;
	__u8 left_present;
	__u8 left_capacity;
	__u8 left_charging;
	__u8 right_present;
	__u8 right_capacity;
	__u8 right_charging;
} __packed;

struct airpods_power_bridge;

struct airpods_power_cell {
	struct airpods_power_bridge *bridge;
	struct power_supply *supply;
	const char *model;
	bool present;
	u8 capacity;
	bool charging;
};

struct airpods_power_bridge {
	struct mutex lock;
	struct delayed_work stale_work;
	struct airpods_power_cell left;
	struct airpods_power_cell right;
};

static struct airpods_power_bridge *bridge;

static enum power_supply_property airpods_power_properties[] = {
	POWER_SUPPLY_PROP_STATUS,
	POWER_SUPPLY_PROP_PRESENT,
	POWER_SUPPLY_PROP_CAPACITY,
	POWER_SUPPLY_PROP_SCOPE,
	POWER_SUPPLY_PROP_MODEL_NAME,
	POWER_SUPPLY_PROP_MANUFACTURER,
};

static int airpods_power_get_property(struct power_supply *supply,
				      enum power_supply_property property,
				      union power_supply_propval *value)
{
	struct airpods_power_cell *cell = power_supply_get_drvdata(supply);

	mutex_lock(&cell->bridge->lock);
	switch (property) {
	case POWER_SUPPLY_PROP_STATUS:
		if (!cell->present)
			value->intval = POWER_SUPPLY_STATUS_UNKNOWN;
		else if (cell->charging)
			value->intval = POWER_SUPPLY_STATUS_CHARGING;
		else if (cell->capacity == 100)
			value->intval = POWER_SUPPLY_STATUS_FULL;
		else
			value->intval = POWER_SUPPLY_STATUS_DISCHARGING;
		break;
	case POWER_SUPPLY_PROP_PRESENT:
		value->intval = cell->present;
		break;
	case POWER_SUPPLY_PROP_CAPACITY:
		value->intval = cell->capacity;
		break;
	case POWER_SUPPLY_PROP_SCOPE:
		value->intval = POWER_SUPPLY_SCOPE_DEVICE;
		break;
	case POWER_SUPPLY_PROP_MODEL_NAME:
		value->strval = cell->model;
		break;
	case POWER_SUPPLY_PROP_MANUFACTURER:
		value->strval = "Apple";
		break;
	default:
		mutex_unlock(&cell->bridge->lock);
		return -EINVAL;
	}
	mutex_unlock(&cell->bridge->lock);
	return 0;
}

static const struct power_supply_desc airpods_left_desc = {
	.name = "airpods_left",
	.type = POWER_SUPPLY_TYPE_BATTERY,
	.properties = airpods_power_properties,
	.num_properties = ARRAY_SIZE(airpods_power_properties),
	.get_property = airpods_power_get_property,
};

static const struct power_supply_desc airpods_right_desc = {
	.name = "airpods_right",
	.type = POWER_SUPPLY_TYPE_BATTERY,
	.properties = airpods_power_properties,
	.num_properties = ARRAY_SIZE(airpods_power_properties),
	.get_property = airpods_power_get_property,
};

static bool airpods_power_valid_bool(u8 value)
{
	return value == 0 || value == 1;
}

static void airpods_power_mark_stale(struct work_struct *work)
{
	struct airpods_power_bridge *target = container_of(
		to_delayed_work(work), struct airpods_power_bridge, stale_work);

	mutex_lock(&target->lock);
	target->left.present = false;
	target->left.capacity = 0;
	target->left.charging = false;
	target->right.present = false;
	target->right.capacity = 0;
	target->right.charging = false;
	mutex_unlock(&target->lock);

	power_supply_changed(target->left.supply);
	power_supply_changed(target->right.supply);
}

static ssize_t airpods_power_write(struct file *file,
				   const char __user *buffer, size_t length,
				   loff_t *offset)
{
	struct airpods_power_update update;

	if (length != sizeof(update))
		return -EMSGSIZE;
	if (copy_from_user(&update, buffer, sizeof(update)))
		return -EFAULT;
	if (update.version != AIRPODS_POWER_PROTOCOL_VERSION)
		return -EPROTO;
	if (!airpods_power_valid_bool(update.left_present) ||
	    !airpods_power_valid_bool(update.left_charging) ||
	    !airpods_power_valid_bool(update.right_present) ||
	    !airpods_power_valid_bool(update.right_charging))
		return -EINVAL;
	if ((update.left_present && update.left_capacity > 100) ||
	    (update.right_present && update.right_capacity > 100))
		return -ERANGE;

	cancel_delayed_work_sync(&bridge->stale_work);
	mutex_lock(&bridge->lock);
	bridge->left.present = update.left_present;
	bridge->left.capacity = update.left_capacity;
	bridge->left.charging = update.left_charging;
	bridge->right.present = update.right_present;
	bridge->right.capacity = update.right_capacity;
	bridge->right.charging = update.right_charging;
	mutex_unlock(&bridge->lock);
	schedule_delayed_work(&bridge->stale_work,
		msecs_to_jiffies(AIRPODS_POWER_STALE_TIMEOUT_MS));

	power_supply_changed(bridge->left.supply);
	power_supply_changed(bridge->right.supply);
	return sizeof(update);
}

static const struct file_operations airpods_power_fops = {
	.owner = THIS_MODULE,
	.write = airpods_power_write,
	.llseek = no_llseek,
};

static struct miscdevice airpods_power_miscdev = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "airpods_power",
	.fops = &airpods_power_fops,
	.mode = 0660,
};

static int airpods_power_register_cell(struct airpods_power_cell *cell,
				       const struct power_supply_desc *description)
{
	struct power_supply_config config = {
		.drv_data = cell,
	};

	cell->supply = power_supply_register(NULL, description, &config);
	return PTR_ERR_OR_ZERO(cell->supply);
}

static int __init airpods_power_init(void)
{
	int result;

	bridge = kzalloc(sizeof(*bridge), GFP_KERNEL);
	if (!bridge)
		return -ENOMEM;

	mutex_init(&bridge->lock);
	INIT_DELAYED_WORK(&bridge->stale_work, airpods_power_mark_stale);
	bridge->left.bridge = bridge;
	bridge->left.model = "AirPods Left";
	bridge->right.bridge = bridge;
	bridge->right.model = "AirPods Right";

	result = airpods_power_register_cell(&bridge->left, &airpods_left_desc);
	if (result)
		goto free_bridge;
	result = airpods_power_register_cell(&bridge->right, &airpods_right_desc);
	if (result)
		goto unregister_left;
	result = misc_register(&airpods_power_miscdev);
	if (result)
		goto unregister_right;

	pr_info("airpods_power: Left and Right battery bridge ready\n");
	return 0;

unregister_right:
	power_supply_unregister(bridge->right.supply);
unregister_left:
	power_supply_unregister(bridge->left.supply);
free_bridge:
	kfree(bridge);
	bridge = NULL;
	return result;
}

static void __exit airpods_power_exit(void)
{
	cancel_delayed_work_sync(&bridge->stale_work);
	misc_deregister(&airpods_power_miscdev);
	power_supply_unregister(bridge->right.supply);
	power_supply_unregister(bridge->left.supply);
	kfree(bridge);
	bridge = NULL;
}

module_init(airpods_power_init);
module_exit(airpods_power_exit);

MODULE_AUTHOR("Abdulloh");
MODULE_DESCRIPTION("Virtual AirPods Left and Right batteries for UPower");
MODULE_LICENSE("GPL");
MODULE_VERSION("0.1.0");
