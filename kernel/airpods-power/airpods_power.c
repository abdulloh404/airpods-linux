// SPDX-License-Identifier: GPL-2.0-only
//! สร้าง power_supply เสมือนสำหรับแบตเตอรี่ AirPods ข้างซ้าย ข้างขวา และเคสชาร์จ
//!
//! daemon เขียน payload protocol 2 ผ่าน misc device แล้ว module สร้างหรือลบ battery device
//! ตาม presence ของแต่ละก้อน โดยลบทั้งหมดเมื่อขาดการอัปเดตครบ 90 วินาที
//! การอ่าน property ใช้ lock ระยะสั้น ส่วนการ register/unregister ใช้ update_lock แยก

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

/// รุ่น payload ที่ต้องตรงกับ byte แรกจาก daemon
#define AIRPODS_POWER_PROTOCOL_VERSION 2
/// เวลารอข้อมูลก่อนนำ battery device ที่อาจค้างออกจากระบบ
#define AIRPODS_POWER_STALE_TIMEOUT_MS 90000

/// รูปแบบที่เขียนครั้งเดียวต่อ update; packed ทำให้ไม่มี padding ระหว่าง field
struct airpods_power_update {
	/// รุ่น protocol ซึ่งปัจจุบันรับเฉพาะค่า 2
	__u8 version;
	/// ระบุว่ามีค่าแบตเตอรี่ซ้ายที่นำไปแสดงได้หรือไม่
	__u8 left_present;
	/// เปอร์เซ็นต์ซ้ายช่วง 0–100 เมื่อ present เป็นจริง
	__u8 left_capacity;
	/// สถานะชาร์จซ้าย โดย boolean ต้องเป็น 0 หรือ 1
	__u8 left_charging;
	/// ระบุว่ามีค่าแบตเตอรี่ขวาที่นำไปแสดงได้หรือไม่
	__u8 right_present;
	/// เปอร์เซ็นต์ขวาช่วง 0–100 เมื่อ present เป็นจริง
	__u8 right_capacity;
	/// สถานะชาร์จขวา โดย boolean ต้องเป็น 0 หรือ 1
	__u8 right_charging;
	/// ระบุว่ามีค่าแบตเตอรี่เคสที่นำไปแสดงได้หรือไม่
	__u8 case_present;
	/// เปอร์เซ็นต์เคสช่วง 0–100 เมื่อ present เป็นจริง
	__u8 case_capacity;
	/// สถานะชาร์จเคส โดย boolean ต้องเป็น 0 หรือ 1
	__u8 case_charging;
	/// ระบุว่าค่าเคสเป็น last known ไม่ใช่ measurement ปัจจุบัน
	__u8 case_stale;
} __packed;

struct airpods_power_bridge;

/// สถานะหนึ่งข้างและ handle ของ power_supply ที่มีอยู่เฉพาะเมื่อ present
struct airpods_power_cell {
	/// กลับไปหา lock และ state ร่วมของ module
	struct airpods_power_bridge *bridge;
	/// handle ที่ register แล้ว หรือ NULL เมื่อไม่ได้เผยแพร่ข้างนี้
	struct power_supply *supply;
	/// ชื่อ model ที่ส่งผ่าน property ให้ UPower
	const char *model;
	/// ระบุว่าค่าจาก daemon ใช้แสดงเป็นอุปกรณ์ได้
	bool present;
	/// เปอร์เซ็นต์ล่าสุดที่รับมา
	u8 capacity;
	/// ระบุว่ากำลังชาร์จตามข้อมูลของ daemon
	bool charging;
	/// ระบุว่าค่านี้มาจาก cache และต้องไม่รายงานว่ากำลังชาร์จ
	bool stale;
};

/// owner กลางสำหรับแบตเตอรี่ทั้งสามก้อน, write serialization และงานหมดอายุ
struct airpods_power_bridge {
	/// ป้องกันข้อมูลที่ property callback อ่านขณะ writer เปลี่ยนค่า
	struct mutex lock;
	/// จัดลำดับ writer/stale worker และการ register/unregister supply
	struct mutex update_lock;
	/// งานล้าง device เมื่อหมดเวลาโดยไม่มี payload ใหม่
	struct delayed_work stale_work;
	/// สถานะและ device ของหูฟังซ้าย
	struct airpods_power_cell left;
	/// สถานะและ device ของหูฟังขวา
	struct airpods_power_cell right;
	/// สถานะและ device ของเคสชาร์จ
	struct airpods_power_cell charging_case;
};

/// instance เดียวตลอดอายุ module; init สร้างและ exit คืนหน่วยความจำ
static struct airpods_power_bridge *bridge;

/// property ที่ module ส่งออกจริง; ไม่มีข้อมูล voltage หรือ energy
static enum power_supply_property airpods_power_properties[] = {
	POWER_SUPPLY_PROP_STATUS,
	POWER_SUPPLY_PROP_PRESENT,
	POWER_SUPPLY_PROP_CAPACITY,
	POWER_SUPPLY_PROP_SCOPE,
	POWER_SUPPLY_PROP_MODEL_NAME,
	POWER_SUPPLY_PROP_MANUFACTURER,
};

/// อ่าน property ภายใต้ data lock และแปลง presence/charging/freshness/capacity เป็น status
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
		else if (cell->charging && !cell->stale)
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

/// ชื่อ sysfs และ callback ของแบตเตอรี่ซ้าย
static const struct power_supply_desc airpods_left_desc = {
	.name = "airpods_left",
	.type = POWER_SUPPLY_TYPE_BATTERY,
	.properties = airpods_power_properties,
	.num_properties = ARRAY_SIZE(airpods_power_properties),
	.get_property = airpods_power_get_property,
};

/// ชื่อ sysfs และ callback ของแบตเตอรี่ขวา
static const struct power_supply_desc airpods_right_desc = {
	.name = "airpods_right",
	.type = POWER_SUPPLY_TYPE_BATTERY,
	.properties = airpods_power_properties,
	.num_properties = ARRAY_SIZE(airpods_power_properties),
	.get_property = airpods_power_get_property,
};

/// ชื่อ sysfs และ callback ของแบตเตอรี่เคสชาร์จ
static const struct power_supply_desc airpods_case_desc = {
	.name = "airpods_case",
	.type = POWER_SUPPLY_TYPE_BATTERY,
	.properties = airpods_power_properties,
	.num_properties = ARRAY_SIZE(airpods_power_properties),
	.get_property = airpods_power_get_property,
};

/// สร้าง power_supply และผูก cell ไว้ให้ property callback เรียกคืน
static int airpods_power_register_cell(struct airpods_power_cell *cell,
				       const struct power_supply_desc *description)
{
	struct power_supply_config config = {
		.drv_data = cell,
	};
	struct power_supply *supply;

	supply = power_supply_register(NULL, description, &config);
	if (IS_ERR(supply))
		return PTR_ERR(supply);
	cell->supply = supply;
	return 0;
}

/// ถอน device ที่มีอยู่และล้าง handle เพื่อให้เรียกซ้ำได้
static void airpods_power_unregister_cell(struct airpods_power_cell *cell)
{
	if (!cell->supply)
		return;
	power_supply_unregister(cell->supply);
	cell->supply = NULL;
}

/// ปรับการมีอยู่ของ device ตาม presence และแจ้งว่าค่า property เปลี่ยนแล้ว
static int airpods_power_sync_cell(struct airpods_power_cell *cell,
				   const struct power_supply_desc *description)
{
	int result;

	if (!cell->present) {
		airpods_power_unregister_cell(cell);
		return 0;
	}
	if (!cell->supply) {
		result = airpods_power_register_cell(cell, description);
		if (result)
			return result;
	}
	power_supply_changed(cell->supply);
	return 0;
}

/// ปฏิเสธค่า boolean ที่อยู่นอก wire format 0/1
static bool airpods_power_valid_bool(u8 value)
{
	return value == 0 || value == 1;
}

/// ล้างค่าและถอนทั้งสอง device เมื่อ delayed work หมดเวลา
static void airpods_power_mark_stale(struct work_struct *work)
{
	struct airpods_power_bridge *target = container_of(
		to_delayed_work(work), struct airpods_power_bridge, stale_work);

	mutex_lock(&target->update_lock);
	mutex_lock(&target->lock);
	target->left.present = false;
	target->left.capacity = 0;
	target->left.charging = false;
	target->left.stale = false;
	target->right.present = false;
	target->right.capacity = 0;
	target->right.charging = false;
	target->right.stale = false;
	target->charging_case.present = false;
	target->charging_case.capacity = 0;
	target->charging_case.charging = false;
	target->charging_case.stale = false;
	mutex_unlock(&target->lock);
	// ถอน device หลังปล่อย data lock เพราะ power_supply อาจเรียก property callback ระหว่างจัดการ device
	airpods_power_unregister_cell(&target->left);
	airpods_power_unregister_cell(&target->right);
	airpods_power_unregister_cell(&target->charging_case);
	mutex_unlock(&target->update_lock);
}

/// รับ update ครบชุด, ตรวจ payload ก่อนเปลี่ยน state แล้ว refresh เวลาอายุข้อมูล
static ssize_t airpods_power_write(struct file *file,
				   const char __user *buffer, size_t length,
				   loff_t *offset)
{
	struct airpods_power_update update;
	int left_result;
	int right_result;
	int case_result;

	// รับหนึ่ง payload ต่อ write เท่านั้น; ไม่มี buffer สำหรับสะสม partial write
	if (length != sizeof(update))
		return -EMSGSIZE;
	if (copy_from_user(&update, buffer, sizeof(update)))
		return -EFAULT;
	if (update.version != AIRPODS_POWER_PROTOCOL_VERSION)
		return -EPROTO;
	if (!airpods_power_valid_bool(update.left_present) ||
	    !airpods_power_valid_bool(update.left_charging) ||
	    !airpods_power_valid_bool(update.right_present) ||
	    !airpods_power_valid_bool(update.right_charging) ||
	    !airpods_power_valid_bool(update.case_present) ||
	    !airpods_power_valid_bool(update.case_charging) ||
	    !airpods_power_valid_bool(update.case_stale))
		return -EINVAL;
	if ((update.left_present && update.left_capacity > 100) ||
	    (update.right_present && update.right_capacity > 100) ||
	    (update.case_present && update.case_capacity > 100))
		return -ERANGE;

	mutex_lock(&bridge->update_lock);
	mutex_lock(&bridge->lock);
	bridge->left.present = update.left_present;
	bridge->left.capacity = update.left_capacity;
	bridge->left.charging = update.left_charging;
	bridge->left.stale = false;
	bridge->right.present = update.right_present;
	bridge->right.capacity = update.right_capacity;
	bridge->right.charging = update.right_charging;
	bridge->right.stale = false;
	bridge->charging_case.present = update.case_present;
	bridge->charging_case.capacity = update.case_capacity;
	bridge->charging_case.charging = update.case_charging;
	bridge->charging_case.stale = update.case_stale;
	mutex_unlock(&bridge->lock);

	// คง update_lock ระหว่าง sync ทั้งสามก้อน แต่เปิดให้ property callback อ่าน state ได้
	left_result = airpods_power_sync_cell(&bridge->left,
					      &airpods_left_desc);
	right_result = airpods_power_sync_cell(&bridge->right,
					       &airpods_right_desc);
	case_result = airpods_power_sync_cell(&bridge->charging_case,
					      &airpods_case_desc);
	// payload ที่ยังมีแบตเตอรี่ก้อนใดก้อนหนึ่งต่ออายุ timer; ถ้าไม่มีเลยก็ไม่ต้องรอ stale
	if (update.left_present || update.right_present || update.case_present)
		mod_delayed_work(system_wq, &bridge->stale_work,
			msecs_to_jiffies(AIRPODS_POWER_STALE_TIMEOUT_MS));
	else
		cancel_delayed_work(&bridge->stale_work);
	mutex_unlock(&bridge->update_lock);

	// state ถูกอัปเดตแล้วแม้การ register บางข้างล้มเหลว; คืน error ให้ daemon รู้ผล write
	if (left_result)
		return left_result;
	if (right_result)
		return right_result;
	if (case_result)
		return case_result;
	return sizeof(update);
}

/// เปิดเฉพาะ write และปิด seek; module owner ป้องกัน unload ขณะยังมี file reference
static const struct file_operations airpods_power_fops = {
	.owner = THIS_MODULE,
	.write = airpods_power_write,
	.llseek = no_llseek,
};

/// จุดรับข้อมูลจาก user session; udev จัดสิทธิ์เข้าถึงเพิ่มเติมด้วย uaccess
static struct miscdevice airpods_power_miscdev = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "airpods_power",
	.fops = &airpods_power_fops,
	.mode = 0660,
};

/// จอง owner/locks/work และเปิด misc device โดยยังไม่สร้าง battery device จนมีข้อมูล
static int __init airpods_power_init(void)
{
	int result;

	bridge = kzalloc(sizeof(*bridge), GFP_KERNEL);
	if (!bridge)
		return -ENOMEM;

	mutex_init(&bridge->lock);
	mutex_init(&bridge->update_lock);
	INIT_DELAYED_WORK(&bridge->stale_work, airpods_power_mark_stale);
	bridge->left.bridge = bridge;
	bridge->left.model = "AirPods Left";
	bridge->right.bridge = bridge;
	bridge->right.model = "AirPods Right";
	bridge->charging_case.bridge = bridge;
	bridge->charging_case.model = "AirPods Case";

	result = misc_register(&airpods_power_miscdev);
	if (result)
		goto free_bridge;

	pr_info("airpods_power: Left, Right and Case battery bridge ready\n");
	return 0;

free_bridge:
	kfree(bridge);
	bridge = NULL;
	return result;
}

/// รอ stale worker จบ, ถอน misc/battery devices แล้วคืน owner เพื่อไม่ให้ callback ใช้ pointer ที่หมดอายุ
static void __exit airpods_power_exit(void)
{
	cancel_delayed_work_sync(&bridge->stale_work);
	misc_deregister(&airpods_power_miscdev);
	airpods_power_unregister_cell(&bridge->charging_case);
	airpods_power_unregister_cell(&bridge->right);
	airpods_power_unregister_cell(&bridge->left);
	kfree(bridge);
	bridge = NULL;
}

module_init(airpods_power_init);
module_exit(airpods_power_exit);

MODULE_AUTHOR("Abdulloh");
MODULE_DESCRIPTION("Virtual AirPods Left, Right and Case batteries for UPower");
MODULE_LICENSE("GPL");
MODULE_VERSION("0.2.0");
