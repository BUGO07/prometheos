use x86::{
    Ring,
    current::segmentation::Descriptor64,
    current::task::TaskStateSegment,
    dtables::{DescriptorTablePointer, lgdt},
    segmentation::{
        BuildDescriptor, CodeSegmentType, DataSegmentType, Descriptor, DescriptorBuilder,
        GateDescriptorBuilder, SegmentDescriptorBuilder, SegmentSelector, load_cs, load_ds,
        load_es, load_fs, load_gs, load_ss,
    },
    task::load_tr,
};

use crate::{println, utils::Singleton};

#[derive(Debug, Copy, Clone)]
#[repr(C, packed)]
struct GlobalDescriptorTable {
    null: Descriptor,
    kernel_code: Descriptor,
    kernel_data: Descriptor,
    user_code_32bit: Descriptor,
    user_data: Descriptor,
    user_code: Descriptor,
    tss: Descriptor64,
}

static GDT: Singleton<GlobalDescriptorTable> = Singleton::new();
static TSS: Singleton<TaskStateSegment> = Singleton::new();

const IST_STACK_SIZE: usize = 4096 * 4;

#[repr(C, align(16))]
struct IstStack([u8; IST_STACK_SIZE]);

static mut DOUBLE_FAULT_STACK: IstStack = IstStack([0; IST_STACK_SIZE]);

pub fn init() {
    println!("init");
    let mut tss = TaskStateSegment::default();
    let stack_bottom = &raw mut DOUBLE_FAULT_STACK as u64;
    let stack_top = stack_bottom + IST_STACK_SIZE as u64;
    tss.set_ist(0, stack_top);
    tss.iomap_base = size_of::<TaskStateSegment>() as u16;
    TSS.install(tss);

    let kernel_code = DescriptorBuilder::code_descriptor(0, 0xFFFFF, CodeSegmentType::ExecuteRead)
        .present()
        .dpl(Ring::Ring0)
        .l() // 64-bit code segment
        .limit_granularity_4kb()
        .finish();

    let kernel_data = DescriptorBuilder::data_descriptor(0, 0xFFFFF, DataSegmentType::ReadWrite)
        .present()
        .dpl(Ring::Ring0)
        .limit_granularity_4kb()
        .finish();

    let user_code_32bit =
        DescriptorBuilder::code_descriptor(0, 0xFFFFF, CodeSegmentType::ExecuteRead)
            .present()
            .dpl(Ring::Ring3)
            .db() // 32-bit compat code
            .limit_granularity_4kb()
            .finish();

    let user_code = DescriptorBuilder::code_descriptor(0, 0xFFFFF, CodeSegmentType::ExecuteRead)
        .present()
        .dpl(Ring::Ring3)
        .l() // 64-bit code segment
        .limit_granularity_4kb()
        .finish();

    let user_data = DescriptorBuilder::data_descriptor(0, 0xFFFFF, DataSegmentType::ReadWrite)
        .present()
        .dpl(Ring::Ring3)
        .limit_granularity_4kb()
        .finish();

    let gdt = GlobalDescriptorTable {
        null: Descriptor::NULL,
        kernel_code,
        kernel_data,
        user_code_32bit,
        user_data,
        user_code,
        tss: TSS.with(|tss| {
            <DescriptorBuilder as GateDescriptorBuilder<u64>>::tss_descriptor(
                tss as *const _ as u64,
                size_of::<TaskStateSegment>() as u64 - 1,
                true,
            )
            .present()
            .dpl(Ring::Ring0)
            .finish()
        }),
    };
    GDT.install(gdt);
    let gdt_ptr = GDT.with(|gdt| DescriptorTablePointer::new(gdt));
    unsafe {
        println!("lgdt");
        lgdt(&gdt_ptr);
        load_cs(SegmentSelector::new(1, Ring::Ring0));
        load_ss(SegmentSelector::new(2, Ring::Ring0));
        let null = SegmentSelector::from_raw(0);
        load_ds(null);
        load_es(null);
        load_fs(null);
        load_gs(null);

        println!("ltr");
        load_tr(SegmentSelector::new(6, Ring::Ring0));
    }
    println!("done");
}
