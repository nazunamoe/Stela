//! The bounded Kbuild reader the native ports select Linux objects with.
#[path = "../build/kbuild.rs"]
mod kbuild;

use kbuild::{Config, Kbuild};
use std::{fs, path::PathBuf};

/// A fresh driver tree holding `files`, each a (path, contents) pair.
fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("stela-kbuild-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (path, text) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    root
}

fn config(header: &str) -> Config {
    Config::parse(header).unwrap()
}

#[test]
fn composes_includes_conditions_and_simple_references() {
    let root = tree(
        "composes",
        &[
            (
                "Kbuild",
                "# SPDX-License-Identifier: MIT\n\
                 ccflags-y += -I $(src)/include\n\
                 ccflags-y += -I $(src)\n\
                 include $(src)/core/Kbuild\n\
                 drv-y := $(core-y)\n\
                 ifdef CONFIG_X86\n\
                 drv-y += x86.o\n\
                 endif\n\
                 drv-$(CONFIG_ACPI) += acpi.o\n\
                 drv-$(CONFIG_DEBUG_FS) += debugfs.o\n\
                 drv-y += main.o\n\
                 obj-$(CONFIG_DRV) += drv.o\n",
            ),
            ("core/Kbuild", "core-y := core/a.o\ncore-y += core/b.o\n"),
        ],
    );
    let kbuild = Kbuild::read(
        &root,
        &config("#define CONFIG_X86 1\n#define CONFIG_ACPI 1\n#define CONFIG_DRV 1\n"),
    )
    .unwrap();
    assert_eq!(
        kbuild.objects("drv").unwrap(),
        ["core/a.o", "core/b.o", "x86.o", "acpi.o", "main.o"]
    );
    assert_eq!(kbuild.objects("obj").unwrap(), ["drv.o"]);
    assert_eq!(kbuild.includes, [root.join("include"), root.join(".")]);
    assert_eq!(
        kbuild.files,
        [root.join("Kbuild"), root.join("core/Kbuild")]
    );
    let without = Kbuild::read(&root, &config("")).unwrap();
    assert_eq!(
        without.objects("drv").unwrap(),
        ["core/a.o", "core/b.o", "main.o"]
    );
    assert!(without.objects("obj").is_err());
}

#[test]
fn reads_a_makefile_with_continuations_rules_and_tree_includes() {
    let root = tree(
        "makefile",
        &[(
            "Makefile",
            "subdir-ccflags-y += $(call cc-option, -Wformat-truncation)\n\
             subdir-ccflags-$(CONFIG_WERROR) += -Werror\n\
             subdir-ccflags-y += -I$(obj) -I$(src)\n\
             hostprogs := gen\n\
             generated := $(obj)/generated/a.c $(obj)/generated/a.h\n\
             quiet_cmd_gen = GEN     $(notdir $(generated))\n\
             \x20     cmd_gen = mkdir -p $(@D); $(obj)/gen $(src)/a.rules $(generated)\n\
             $(obj)/generated/%.c $(obj)/generated/%.h: $(obj)/gen \\\n\
             \t\t $(src)/a.rules\n\
             \t$(call cmd,gen)\n\
             \n\
             drv-y += a.o \\\n\
             \tb.o\n\
             drv-$(CONFIG_I2C)\t+= i2c.o \\\n\
             \t\t   amc.o\n\
             ifdef CONFIG_OFF\n\
             $(obj)/off/%.o: $(src)/%.c\n\
             \t$(call cmd,off)\n\
             off_only := 1\n\
             endif\n\
             subdir-ccflags-$(CONFIG_DISPLAY) += \\\n\
             \t-I$(src)/compat \\\n\
             \t-I$(srctree)/drivers/shared/display/\n\
             drv-$(CONFIG_DISPLAY) += \\\n\
             \tdisplay/d.o \\\n\
             \tshared-display/s.o\n\
             ifeq ($(CONFIG_ACPI),y)\n\
             \tdrv-$(CONFIG_DISPLAY) += \\\n\
             \t\tshared-display/acpi.o\n\
             endif\n\
             ifneq ($(CONFIG_DISPLAY),y)\n\
             \tfind_args += -not -path display/\\*\n\
             endif\n\
             always-$(CONFIG_WERROR) += $(patsubst %.h,%.hdrtest, $(shell ls))\n\
             uses := $(addprefix $(obj)/, $(drv-y))\n\
             $(uses): $(obj)/generated/a.h\n",
        )],
    );
    let kbuild = Kbuild::read(
        &root,
        &config("#define CONFIG_I2C 1\n#define CONFIG_DISPLAY 1\n"),
    )
    .unwrap();
    assert_eq!(
        kbuild.objects("drv").unwrap(),
        [
            "a.o",
            "b.o",
            "i2c.o",
            "amc.o",
            "display/d.o",
            "shared-display/s.o"
        ]
    );
    assert_eq!(kbuild.includes, [root.join("."), root.join("compat")]);
    assert_eq!(
        kbuild.tree_includes,
        [PathBuf::from("drivers/shared/display")]
    );
    assert!(kbuild.generated_include && !kbuild.werror);
    assert_eq!(kbuild.optional_flags, ["-Wformat-truncation"]);
    assert_eq!(
        kbuild
            .definitions
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>(),
        [
            (
                "cmd_gen",
                "mkdir -p $(@D); $(obj)/gen $(src)/a.rules $(generated)"
            ),
            ("generated", "$(obj)/generated/a.c $(obj)/generated/a.h"),
            ("hostprogs", "gen"),
            ("quiet_cmd_gen", "GEN     $(notdir $(generated))"),
            ("uses", "$(addprefix $(obj)/, $(drv-y))"),
        ]
    );
    assert_eq!(
        kbuild.rules,
        [
            kbuild::Rule {
                targets: "$(obj)/generated/%.c $(obj)/generated/%.h".into(),
                prerequisites: "$(obj)/gen $(src)/a.rules".into(),
                recipe: vec!["$(call cmd,gen)".into()],
            },
            kbuild::Rule {
                targets: "$(uses)".into(),
                prerequisites: "$(obj)/generated/a.h".into(),
                recipe: Vec::new(),
            },
        ]
    );
    let other = Kbuild::read(
        &root,
        &config("#define CONFIG_ACPI 1\n#define CONFIG_WERROR 1\n"),
    );
    // An enabled list holding a function call is refused.
    assert!(other.is_err());
    // A directory's Kbuild is read in place of its Makefile.
    fs::write(root.join("Kbuild"), "drv-y := k.o\n").unwrap();
    let kbuild = Kbuild::read(&root, &config("")).unwrap();
    assert_eq!(kbuild.objects("drv").unwrap(), ["k.o"]);
    assert_eq!(kbuild.files, [root.join("Kbuild")]);
}

#[test]
fn reads_defines_late_references_and_dashed_definitions() {
    // The forms i915's Makefile adds to xe's.
    let root = tree(
        "late",
        &[
            (
                "Makefile",
                "subdir-ccflags-y += -DI915\n\
                 subdir-ccflags-$(CONFIG_RT) += -DNOTRACE\n\
                 drv-y += core.o\n\
                 gt-y += gt/a.o \\\n\
                 \tgt/b.o\n\
                 gt-$(CONFIG_X86) += gt/x86.o\n\
                 drv-y += $(gt-y)\n\
                 include $(src)/sub/Makefile\n\
                 no-header-test := \\\n\
                 \tdisplay/defs.h\n\
                 ifdef CONFIG_WERROR\n\
                 \x20   cmd_checkdoc = $(KERNELDOC) $<\n\
                 endif\n",
            ),
            ("sub/Makefile", "kvmgt-$(CONFIG_GVT) += sub/a.o\n"),
        ],
    );
    let kbuild = Kbuild::read(&root, &config("")).unwrap();
    assert_eq!(
        kbuild.objects("drv").unwrap(),
        ["core.o", "gt/a.o", "gt/b.o"]
    );
    assert_eq!(kbuild.defines, ["I915"]);
    assert!(kbuild.objects("kvmgt").is_err());
    assert_eq!(
        kbuild
            .definitions
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>(),
        [("no-header-test", "display/defs.h")]
    );
    // A define of no plain name is refused.
    fs::write(root.join("Makefile"), "subdir-ccflags-y += -DA=1\n").unwrap();
    assert!(Kbuild::read(&root, &config("")).is_err());
}

#[test]
fn refuses_what_it_cannot_evaluate_even_when_inactive() {
    for (name, text, header) in [
        ("unknown", "ifdef CONFIG_OFF\nifeq ($(A),b)\nendif\n", ""),
        ("shell", "drv-y += $(shell ls)\n", ""),
        ("pattern", "drv-y += $(patsubst %.c,%.o,a.c)\n", ""),
        ("escape", "include $(src)/../Kbuild\n", ""),
        ("source", "drv-y += core/a.c\n", ""),
        ("recursive", "a-y += a.o\ndrv-y += $(a-y)\na-y += b.o\n", ""),
        ("undefined", "drv-y := $(missing-y)\n", ""),
        (
            "module",
            "drv-$(CONFIG_M) += m.o\n",
            "#define CONFIG_M_MODULE 1\n",
        ),
        (
            "string",
            "drv-$(CONFIG_S) += s.o\n",
            "#define CONFIG_S \"s\"\n",
        ),
        ("unbalanced", "ifdef CONFIG_X86\ndrv-y += a.o\n", ""),
        ("stray", "endif\n", ""),
        ("flag", "ccflags-y += -O2\n", ""),
        ("define value", "ccflags-y += -DDEBUG=1\n", ""),
        ("module list", "drv-m += a.o\n", ""),
        ("option", "ccflags-y += $(call cc-option, -DDEBUG)\n", ""),
        ("assigned flags", "ccflags-y := -I$(src)\n", ""),
        ("outside", "ccflags-y += -I$(srctree)/../include\n", ""),
        ("absolute", "ccflags-y += -I/usr/include\n", ""),
        ("compare", "ifeq ($(CONFIG_A),m)\nendif\n", ""),
        ("negated", "ifneq ($(A),y)\nendif\n", ""),
        ("else", "ifdef CONFIG_A\nelse\nendif\n", ""),
        ("recursive list", "drv-y = a.o\n", ""),
        ("dashed", "drv-objs := a.o\n", ""),
        ("word", "export\n", ""),
        ("loop", "include $(src)/Kbuild\n", ""),
    ] {
        let root = tree(name, &[("Kbuild", text)]);
        assert!(
            Kbuild::read(&root, &config(header)).is_err(),
            "{name} was accepted"
        );
    }
    assert!(Config::parse("#define CONFIG_A 1\n#define CONFIG_A 1\n").is_err());
}
