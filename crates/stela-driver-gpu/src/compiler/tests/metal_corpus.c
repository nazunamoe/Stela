/* SPDX-License-Identifier: GPL-2.0-or-later */
/* Compiles every entry point of AIR modules the way the Metal device does,
 * with NAK's NIR options, or with BRW's for Intel, and reports each one's
 * outcome: the shader coverage sweep of the Metal device's bring-up. Each
 * argument is an .air module from Apple's `metal -c`; the inputs are never
 * committed.
 *
 *   stela_metal_corpus [--sm SM] [--fill BYTE] [--nak 1] [--intel VER] MODULE.air...
 *
 * A kernel takes a 64-thread threadgroup and every threadgroup buffer its
 * declared bytes; a render function is linked with the varyings it writes
 * or reads;
 * a post-tessellation vertex function takes its patch's default
 * tessellation, and each of its attributes a buffer of its own; required
 * function constants take zero, and with --fill every function constant
 * takes that byte, reaching what the constants otherwise disable. With
 * --nak 1, each kernel without textures or samplers is also compiled by
 * NAK, its push constants read as global constants, and a line reports
 * NAK's statistics and its global and local memory instructions by order:
 * a census of the code, never a measurement of a GPU.
 *
 * With --intel VER, the sweep runs once for each Intel compiler generation
 * (verx10) of Mesa's platform table from Gen VER, with the first platform
 * Mesa names for it, through BRW: Gen9 onward, since ELK compiles Gen7 and
 * Gen8. Each sweep takes ANV's NIR options and limits for that device, and
 * each kernel without textures or samplers is also compiled by BRW as ANV
 * compiles a Metal kernel, at its 32-lane thread execution width, with the
 * same stand-in bindings and ANV's driver values stood in alike; a line
 * reports BRW's statistics. Each BRW compile runs in a child process, so an
 * assertion is reported as an abort and the sweep goes on. A held platform
 * says so and is skipped. Lines of a sweep start with its platform.
 *
 * Exits nonzero when any entry point is refused. */
#include "compiler/glsl_types.h"
#include "compiler/metal_compiler.h"
#include "compiler/nir/nir_builder.h"
#include "llvm-c/BitReader.h"
#include "llvm-c/Core.h"
#include "util/macros.h"
#ifdef STELA_CORPUS_NAK
#include "nouveau/compiler/nak.h"
#include "nouveau/headers/nv_device_info.h"
#endif
#ifdef STELA_CORPUS_BRW
#include "compiler/tests/targets/intel.h"
#include "intel/compiler/brw/brw_compiler.h"
#include "intel/compiler/brw/brw_nir.h"
#include "util/ralloc.h"
#include <sys/wait.h>
#include <unistd.h>
#endif
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define WRAPPER_MAGIC 0x0b17c0deu

static struct stela_metal_reflection reflection;
static struct stela_air_constant_value constants[STELA_METAL_FUNCTION_CONSTANTS];
static unsigned constant_count;
static int fill = -1;
/* What starts each line of the sweep: empty, or an Intel platform's name. */
static char tag[32];
#ifdef STELA_CORPUS_NAK
static struct nak_compiler *census;
#endif
#ifdef STELA_CORPUS_BRW
static struct brw_compiler *brw;
static struct intel_device_info brw_device;
#endif

/* The module's bitcode, unwrapped from an LLVM bitcode wrapper. */
static const uint8_t *
bitcode(const uint8_t *bytes, size_t *size)
{
   uint32_t words[5];
   if (*size < sizeof(words))
      return bytes;
   memcpy(words, bytes, sizeof(words));
   if (words[0] != WRAPPER_MAGIC || words[2] > *size || words[3] > *size - words[2])
      return bytes;
   *size = words[3];
   return bytes + words[2];
}

/* Bytes of a function constant of an MSL type: its scalar's, times its
 * lanes. */
static size_t
constant_bytes(const char *type, size_t length)
{
   static const struct { const char *name; size_t bytes; } scalars[] = {
      {"bool", 1}, {"char", 1}, {"uchar", 1}, {"short", 2}, {"ushort", 2}, {"half", 2},
      {"bfloat", 2}, {"int", 4}, {"uint", 4}, {"float", 4}, {"long", 8}, {"ulong", 8},
   };
   for (unsigned i = 0; i < sizeof(scalars) / sizeof(scalars[0]); i++) {
      size_t name = strlen(scalars[i].name);
      if (length < name || memcmp(type, scalars[i].name, name))
         continue;
      if (length == name)
         return scalars[i].bytes;
      unsigned lanes = (unsigned)(type[name] - '0');
      if (length == name + 1 && lanes >= 2 && lanes <= 4)
         return scalars[i].bytes * (lanes == 3 ? 4 : lanes);
   }
   return 16;
}

/* The value of each constant reflection lists that takes one. */
static void
choose_constants(void)
{
   static uint8_t bytes[16];
   memset(bytes, fill < 0 ? 0 : fill, sizeof(bytes));
   constant_count = 0;
   for (unsigned i = 0; i < reflection.function_constant_count; i++) {
      const struct stela_metal_function_constant *constant = &reflection.function_constants[i];
      if (constant->required || fill >= 0)
         constants[constant_count++] = (struct stela_air_constant_value){
            constant->index, bytes, constant_bytes(reflection.names + constant->type,
                                                   constant->type_bytes)};
   }
}

/* The MTLVertexFormat of 32-bit lanes that carries an attribute. */
static uint32_t
attribute_format(const struct stela_metal_attribute_use *attribute)
{
   uint32_t lanes = attribute->components;
   switch (attribute->primitive) {
   case STELA_AIR_FLOAT: case STELA_AIR_HALF: return 27 + lanes; /* Float */
   case STELA_AIR_UINT: case STELA_AIR_USHORT: case STELA_AIR_UCHAR: return 35 + lanes; /* UInt */
   default: return 31 + lanes; /* Int */
   }
}

/* Compiles entry, which reflected into reflection: NULL after reporting
 * why in error. */
static nir_shader *
compile(const uint8_t *air, size_t size, const char *entry,
        const struct stela_metal_target *target, bool tile, struct stela_air_error *error)
{
   struct stela_metal_specialization specialization = {.max_call_stack_depth = 1};
   uint32_t stage = reflection.stage;
   if (stage == STELA_METAL_KERNEL || stage == STELA_METAL_OBJECT || stage == STELA_METAL_MESH) {
      unsigned threads = reflection.max_total_threads && reflection.max_total_threads < 64 ?
                            reflection.max_total_threads : 64;
      specialization.workgroup_size[0] = threads;
      specialization.workgroup_size[1] = specialization.workgroup_size[2] = 1;
   }
   static struct stela_metal_threadgroup_length lengths[STELA_METAL_BUFFERS];
   for (unsigned i = 0; i < reflection.threadgroup_count; i++) {
      uint32_t bytes = reflection.threadgroup[i].bytes ? reflection.threadgroup[i].bytes : 16;
      lengths[i] = (struct stela_metal_threadgroup_length){reflection.threadgroup[i].index,
                                                           (bytes + 15) & ~15u};
   }
   specialization.threadgroup_lengths = lengths;
   specialization.threadgroup_length_count = reflection.threadgroup_count;
   static struct stela_metal_linked_varying varyings[STELA_METAL_VARYINGS];
   for (unsigned i = 0; i < reflection.varying_count; i++) {
      const struct stela_metal_varying *varying = &reflection.varyings[i];
      varyings[i] = (struct stela_metal_linked_varying){
         reflection.names + varying->identifier, varying->identifier_bytes, varying->per_primitive};
   }
   specialization.varyings = varyings;
   specialization.varying_count = reflection.varying_count;
   specialization.constants = constants;
   specialization.constant_count = constant_count;
   const struct stela_metal_tessellation tessellation = {
      .quad = reflection.patch == STELA_AIR_PATCH_QUAD, .max_factor = 16,
      .partition = STELA_METAL_PARTITION_POW2, .step = STELA_METAL_FACTOR_STEP_PER_PATCH,
   };
   static struct stela_metal_stage_in stage_in;
   memset(&stage_in, 0, sizeof(stage_in));
   for (unsigned i = 0; i < reflection.attribute_count; i++) {
      const struct stela_metal_attribute_use *attribute = &reflection.attributes[i];
      if (!attribute->active)
         continue;
      stage_in.attributes[attribute->index] = (struct stela_metal_stage_in_attribute){
         attribute_format(attribute), 0, attribute->index};
      stage_in.layouts[attribute->index] = (struct stela_metal_stage_in_layout){
         16, attribute->patch_control_point ? STELA_METAL_STEP_PER_PATCH_CONTROL_POINT :
             reflection.patch ? STELA_METAL_STEP_PER_PATCH : STELA_METAL_STEP_THREAD_POSITION_IN_GRID_X,
         1};
   }
   /* Vertex functions fetch through the pipeline's vertex input instead. */
   if (reflection.attribute_count && (stage == STELA_METAL_KERNEL || reflection.patch))
      specialization.stage_in = &stage_in;
   if (reflection.patch)
      specialization.tessellation = &tessellation;
   /* Each constexpr sampler state a sampler of its own, as the device
    * creates them. */
   static uint64_t states[STELA_METAL_SAMPLER_STATES];
   static struct stela_air_constexpr_sampler samplers[STELA_METAL_SAMPLER_STATES];
   if (!stela_metal_sampler_states(air, size, states, &specialization.sampler_count, error))
      return NULL;
   for (unsigned i = 0; i < specialization.sampler_count; i++)
      samplers[i] = (struct stela_air_constexpr_sampler){states[i], (uint64_t)i << 32 | i};
   specialization.samplers = samplers;
   if (tile) {
      /* As a tile render pipeline compiles it (pipeline.rs): a 16 x 16
       * tile's threads in one linear workgroup of dynamic threadgroups,
       * over half4 color attachments, one sample each. */
      static const struct stela_metal_attachment colors[8] = {
         [0 ... 7] = {.format = 97 /* VK_FORMAT_R16G16B16A16_SFLOAT */, .bytes = 8,
                      .write_mask = 0xf},
      };
      specialization.workgroup_size[0] = 256;
      specialization.dynamic_threadgroups = 1;
      specialization.threadgroup_length_count = 0;
      specialization.tile = 1;
      specialization.samples = 1;
      specialization.attachments = colors;
      specialization.attachment_count = 8;
   }
   return stela_metal_compile(air, size, entry, target, &specialization, error);
}

/* Push constants as global constant loads and images as bindless handles,
 * the bindings the driver's lowering gives a kernel. */
static bool
stand_in_bindings(nir_builder *b, nir_intrinsic_instr *intrin, void *data)
{
   (void)data;
   b->cursor = nir_before_instr(&intrin->instr);
   switch (intrin->intrinsic) {
   case nir_intrinsic_load_push_constant: {
      nir_def *address = nir_iadd_imm(b, nir_u2u64(b, intrin->src[0].ssa),
                                      0x100000 + nir_intrinsic_base(intrin));
      nir_def_replace(&intrin->def, nir_load_global_constant(b, intrin->def.num_components,
                                                             intrin->def.bit_size, address,
                                                             .align_mul = 4));
      return true;
   }
   case nir_intrinsic_image_deref_load:
   case nir_intrinsic_image_deref_store:
   case nir_intrinsic_image_deref_size:
   case nir_intrinsic_image_deref_samples:
   case nir_intrinsic_image_deref_atomic:
   case nir_intrinsic_image_deref_atomic_swap: {
      nir_deref_instr *deref = nir_src_as_deref(intrin->src[0]);
      nir_def *handle = deref->deref_type == nir_deref_type_array ?
                           nir_u2u32(b, deref->arr.index.ssa) : nir_imm_int(b, 0);
      nir_rewrite_image_intrinsic(intrin, handle, nir_image_intrinsic_type_bindless);
      return true;
   }
   default:
      return false;
   }
}

/* A kernel's bindings as the driver's lowering gives them, and its dead
 * resources gone, before a census compiles it. */
static void
stand_in(nir_shader *shader)
{
   nir_shader_intrinsics_pass(shader, stand_in_bindings, nir_metadata_control_flow, NULL);
   nir_opt_dce(shader);
   nir_remove_dead_derefs(shader);
   nir_remove_dead_variables(shader, nir_var_image | nir_var_uniform, NULL);
}

static bool
samples(nir_shader *shader)
{
   nir_foreach_function_impl(impl, shader) {
      nir_foreach_block(block, impl) {
         nir_foreach_instr(instr, block) {
            if (instr->type == nir_instr_type_tex)
               return true;
         }
      }
   }
   return false;
}

#ifdef STELA_CORPUS_NAK
/* Lines of a NAK listing holding `op` and, after it, `order`. */
static unsigned
count(const char *listing, const char *op, const char *order)
{
   unsigned n = 0;
   for (const char *line = listing; line && *line;) {
      const char *end = strchr(line, '\n');
      char text[256];
      snprintf(text, sizeof(text), "%.*s", end ? (int)(end - line) : (int)strlen(line), line);
      const char *at = strstr(text, op);
      n += at && (!order || strstr(at, order));
      line = end ? end + 1 : NULL;
   }
   return n;
}

/* NAK's statistics for a translated kernel; consumes shader's code. */
static void
report_nak(const char *path, const char *entry, nir_shader *shader)
{
   if (reflection.stage != STELA_METAL_KERNEL || samples(shader)) {
      printf("%s %s: nak skipped\n", path, entry);
      return;
   }
   stand_in(shader);
   const nir_lower_compute_system_values_options system_values = {
      .lower_local_invocation_index = true,
   };
   nir_lower_compute_system_values(shader, &system_values);
   nak_preprocess_nir(shader, census);
   struct nak_shader_bin *bin = nak_compile_shader(shader, true, census, 0, NULL, false);
   if (!bin) {
      printf("%s %s: nak refused\n", path, entry);
      return;
   }
   const char *a = bin->asm_str;
   printf("%s %s: nak instrs=%u static_cycles=%llu gprs=%u warps=%u slm=%u spills=%u fills=%u "
          "st.weak=%u st.strong=%u ld.weak=%u ld.strong=%u ld.constant=%u local=%u\n",
          path, entry, bin->info.num_instrs, (unsigned long long)bin->info.num_static_cycles,
          bin->info.num_gprs, bin->info.max_warps_per_sm, bin->info.slm_size,
          bin->info.num_spills_to_mem, bin->info.num_fills_from_mem,
          count(a, "st.global", ".weak"), count(a, "st.global", ".strong"),
          count(a, "ld.global", ".weak"), count(a, "ld.global", ".strong"),
          count(a, "ld.global", ".constant"), count(a, ".local", NULL));
   nak_shader_bin_destroy(bin);
}
#endif

#ifdef STELA_CORPUS_BRW
static void
quiet(void *data, unsigned *id, const char *format, ...)
{
   (void)data;
   (void)id;
   (void)format;
}

/* The values ANV's lowering reads from its driver uniforms and its
 * shader's constant data (anv_nir_lower_driver_values.c), as global
 * constant loads of stand-in addresses, the bindings a census needs. */
static bool
stand_in_driver_values(nir_builder *b, nir_intrinsic_instr *intrin, void *data)
{
   (void)data;
   b->cursor = nir_before_instr(&intrin->instr);
   switch (intrin->intrinsic) {
   case nir_intrinsic_load_num_workgroups:
      nir_def_replace(&intrin->def, nir_load_global_constant(b, 3, 32, nir_imm_int64(b, 0xff000),
                                                             .align_mul = 4));
      return true;
   case nir_intrinsic_load_global_constant_bounded: {
      /* As anv_nir_lower_ubo_loads.c's dynamic form: an access past the
       * bound reads a null address's. */
      nir_def *offset = intrin->src[1].ssa;
      unsigned bytes = intrin->def.bit_size / 8 * intrin->def.num_components;
      nir_def *inside = nir_uge(b, intrin->src[2].ssa, nir_iadd_imm(b, offset, bytes));
      nir_def *address = nir_bcsel(b, inside, nir_iadd(b, intrin->src[0].ssa, nir_u2u64(b, offset)),
                                   nir_imm_int64(b, 0));
      nir_def_replace(&intrin->def, nir_load_global_constant(b, intrin->def.num_components,
                                                             intrin->def.bit_size, address,
                                                             .access = nir_intrinsic_access(intrin),
                                                             .align_mul = nir_intrinsic_align_mul(intrin),
                                                             .align_offset = nir_intrinsic_align_offset(intrin)));
      return true;
   }
   case nir_intrinsic_load_constant: {
      nir_def *address = nir_iadd_imm(b, nir_u2u64(b, intrin->src[0].ssa),
                                      0x200000 + nir_intrinsic_base(intrin));
      nir_def_replace(&intrin->def, nir_load_global_constant(b, intrin->def.num_components,
                                                             intrin->def.bit_size, address,
                                                             .align_mul = 4));
      return true;
   }
   default:
      return false;
   }
}

/* BRW's statistics for a translated kernel, compiled as ANV compiles a
 * compute pipeline of the Metal device: at its required subgroup size, the
 * thread execution width ANV reports (BRW_SUBGROUP_SIZE). The subgroup ID
 * comes in a per-thread push value, as ANV's push layout supplies it. */
static void
report_brw(const char *path, const char *entry, nir_shader *shader)
{
   if (reflection.stage != STELA_METAL_KERNEL || samples(shader)) {
      printf("%s%s %s: brw skipped\n", tag, path, entry);
      return;
   }
   /* BRW asserts on what it does not know; a child compiles, so an
    * assertion is a line of the census rather than its end. */
   fflush(stdout);
   pid_t child = fork();
   if (child < 0) {
      printf("%s%s %s: brw not run: no process\n", tag, path, entry);
      return;
   }
   if (child) {
      int status = 0;
      while (waitpid(child, &status, 0) < 0)
         ;
      if (!WIFEXITED(status) || WEXITSTATUS(status))
         printf("%s%s %s: brw aborted (%s %d)\n", tag, path, entry,
                WIFSIGNALED(status) ? "signal" : "status",
                WIFSIGNALED(status) ? WTERMSIG(status) : WEXITSTATUS(status));
      return;
   }
   stand_in(shader);
   nir_shader_intrinsics_pass(shader, stand_in_driver_values, nir_metadata_none, NULL);
   nir_lower_compute_system_values(shader, NULL);
   const struct brw_nir_compiler_opts options = {0};
   brw_preprocess_nir(brw, shader, &options);
   nir_shader_gather_info(shader, nir_shader_get_entrypoint(shader));
   shader->info.min_subgroup_size = BRW_SUBGROUP_SIZE;
   shader->info.max_subgroup_size = BRW_SUBGROUP_SIZE;
   struct brw_cs_prog_key key = {0};
   struct brw_cs_prog_data data = {0};
   brw_nir_lower_cs_intrinsics(shader, &brw_device, &data);
   if (brw_nir_lower_cs_subgroup_id(shader, &brw_device, 0)) {
      data.base.push_sizes[0] = 4;
      brw_cs_fill_push_const_info(&brw_device, &data, 0);
      data.base.push_sizes[0] = REG_SIZE;
   } else {
      brw_cs_fill_push_const_info(&brw_device, &data, -1);
   }
   /* One for each program BRW emits, at most BRW_TO_BINARY_MAX_SHADERS. */
   struct genisa_stats stats[4] = {0};
   void *context = ralloc_context(NULL);
   struct brw_compile_cs_params parameters = {
      .base = {.mem_ctx = context, .nir = shader, .key = &key.base, .prog_data = &data.base,
               .stats = stats},
   };
   const unsigned *code = brw_compile(brw, &parameters.base);
   if (!code || !data.base.program_size)
      printf("%s%s %s: brw refused: %s\n", tag, path, entry,
             parameters.base.error_str ? parameters.base.error_str : "no code");
   else
      printf("%s%s %s: brw simd=%u instrs=%u bytes=%u sends=%u loops=%u cycles=%u "
             "spills=%u fills=%u scratch=%u\n",
             tag, path, entry, stats[0].dispatch_width, stats[0].instrs, data.base.program_size,
             stats[0].sends, stats[0].loops, stats[0].cycles, stats[0].spills, stats[0].fills,
             data.base.total_scratch);
   fflush(stdout);
   _exit(0);
}
#endif

static unsigned
sweep(const char *path, const struct stela_metal_target *target)
{
   FILE *file = fopen(path, "rb");
   if (!file) {
      printf("%s%s: unreadable\n", tag, path);
      return 1;
   }
   fseek(file, 0, SEEK_END);
   size_t size = (size_t)ftell(file);
   fseek(file, 0, SEEK_SET);
   uint8_t *bytes = malloc(size ? size : 1);
   size_t read = bytes ? fread(bytes, 1, size, file) : 0;
   fclose(file);
   if (read != size) {
      free(bytes);
      printf("%s%s: unreadable\n", tag, path);
      return 1;
   }
   const uint8_t *air = bitcode(bytes, &size);
   LLVMContextRef context = LLVMContextCreate();
   LLVMMemoryBufferRef buffer = LLVMCreateMemoryBufferWithMemoryRange((const char *)air, size,
                                                                     path, false);
   LLVMModuleRef module = NULL;
   unsigned refused = 0;
   if (LLVMParseBitcodeInContext2(context, buffer, &module)) {
      printf("%s%s: not bitcode\n", tag, path);
      refused++;
   }
   for (LLVMValueRef fn = module ? LLVMGetFirstFunction(module) : NULL; fn;
        fn = LLVMGetNextFunction(fn)) {
      size_t name_bytes;
      const char *name = LLVMGetValueName2(fn, &name_bytes);
      if (LLVMIsDeclaration(fn) || !name_bytes)
         continue;
      char *entry = strndup(name, name_bytes);
      struct stela_air_error error;
      bool reflected = stela_metal_reflect(air, size, entry, NULL, 0, &reflection, &error);
      if (reflected) {
         choose_constants();
         reflected = stela_metal_reflect(air, size, entry, constants, constant_count, &reflection,
                                         &error);
      }
      if (!reflected) {
         /* Helpers and other functions no entry metadata lists. */
         if (!strstr(error.detail, "not a listed AIR function"))
            printf("%s%s %s: reflection refused (%u): %s\n", tag, path, entry, error.kind,
                   error.detail),
               refused++;
      } else if (reflection.stage != STELA_METAL_VISIBLE) {
         nir_shader *shader = compile(air, size, entry, target, false, &error);
         /* A kernel of an implicit imageblock is a tile function, which only
          * a tile render pipeline compiles. */
         bool tile = !shader && strstr(error.detail, "implicit imageblock outside a tile function");
         if (tile)
            shader = compile(air, size, entry, target, true, &error);
         if (shader) {
            printf("%s%s %s: ok%s\n", tag, path, entry, tile ? " (tile)" : "");
#ifdef STELA_CORPUS_NAK
            if (census && !tile)
               report_nak(path, entry, shader);
#endif
#ifdef STELA_CORPUS_BRW
            if (brw && !tile)
               report_brw(path, entry, shader);
#endif
            stela_metal_release(shader);
         } else {
            printf("%s%s %s: refused (%u): %s\n", tag, path, entry, error.kind, error.detail);
            refused++;
         }
      }
      free(entry);
   }
   if (module)
      LLVMDisposeModule(module);
   else
      LLVMDisposeMemoryBuffer(buffer);
   LLVMContextDispose(context);
   free(bytes);
   return refused;
}

#ifdef STELA_CORPUS_BRW
/* One sweep for each Intel compiler generation from Gen first, with ANV's
 * NIR options and limits for its first named platform; the refusals. */
static unsigned
sweep_intel(int first, int count, char **paths)
{
   unsigned refused = 0, generations[64], seen = 0;
   const char *name;
   STELA_INTEL_FOREACH_TARGET(name, brw_device) {
      bool again = false;
      for (unsigned i = 0; i < seen; i++)
         again |= generations[i] == brw_device.verx10;
      if (brw_device.ver < (unsigned)first || stela_intel_elk(&brw_device) || again ||
          seen == ARRAY_SIZE(generations))
         continue;
      generations[seen++] = brw_device.verx10;
      snprintf(tag, sizeof(tag), "%s ", name);
      const char *hold = stela_intel_hold(&brw_device);
      if (hold) {
         printf("%sheld: %s\n", tag, hold);
         continue;
      }
      /* The kernel reports the alignment of GPU memory (xe: 4 KiB, or
       * 64 KiB for VRAM that needs it); BRW keeps loads within its pages.
       * A device of Mesa's table has none, so it takes the smaller. */
      if (!brw_device.mem_alignment)
         brw_device.mem_alignment = 4096;
      void *context = ralloc_context(NULL);
      brw = brw_compiler_create(context, &brw_device);
      if (!brw) {
         printf("%sBRW refused the device\n", tag);
         ralloc_free(context);
         refused++;
         continue;
      }
      brw->shader_debug_log = quiet;
      brw->shader_perf_log = quiet;
      /* As ANV reports them (anv_physical_device.c) and the Metal device
       * takes them (metal/compiler.rs). */
      const bool mesh = brw_device.has_mesh_shading;
      struct stela_metal_target target = {
         .max_instructions = 1u << 20,
         .max_threadgroup_bytes = MIN3(48u * 1024, intel_device_info_get_max_slm_size(&brw_device),
                                       UINT16_MAX),
         .max_private_bytes = 1u << 16,
         .max_constant_bytes = intel_indirect_ubos_use_sampler(&brw_device) ? 1u << 27 : 1u << 30,
         .max_varying_slots = MIN2(128, 116) / 4,
         .max_payload_bytes = mesh ? 16384 : 0, .max_mesh_vertices = mesh ? 256 : 0,
         .max_mesh_primitives = mesh ? 256 : 0,
         .stencil_export = 1, .raster_order_groups = 1, .fragment_tests = 1,
         /* ANV adds floats atomically in buffers with LSC messages, and
          * never in shared memory. */
         .float_add_by_swap = STELA_METAL_FLOAT_ADD_THREADGROUP |
                              (brw_device.has_lsc ? 0 : STELA_METAL_FLOAT_ADD_DEVICE),
      };
      for (unsigned stage = 0; stage < STELA_METAL_NIR_STAGES; stage++)
         target.nir[stage] = &brw->nir_options[stage];
      unsigned platform = 0;
      for (int i = 0; i < count; i++)
         platform += sweep(paths[i], &target);
      printf("%s%u refused\n", tag, platform);
      refused += platform;
      ralloc_free(context);
      brw = NULL;
   }
   tag[0] = 0;
   return refused;
}
#endif

int
main(int argc, char **argv)
{
   unsigned sm = 120;
   bool nak_census = false;
   int intel = 0;
   int first = 1;
   while (first + 1 < argc && !strncmp(argv[first], "--", 2)) {
      if (!strcmp(argv[first], "--sm"))
         sm = (unsigned)atoi(argv[first + 1]);
      else if (!strcmp(argv[first], "--fill"))
         fill = atoi(argv[first + 1]);
      else if (!strcmp(argv[first], "--nak"))
         nak_census = atoi(argv[first + 1]) != 0;
      else if (!strcmp(argv[first], "--intel"))
         intel = atoi(argv[first + 1]);
      else
         break;
      first += 2;
   }
#ifndef STELA_CORPUS_BRW
   if (intel) {
      fprintf(stderr, "this build has no Intel compiler\n");
      return 2;
   }
#endif
#ifndef STELA_CORPUS_NAK
   if (!intel) {
      fprintf(stderr, "this build has no NVIDIA compiler: pass --intel VER\n");
      return 2;
   }
#endif
   if (first >= argc || (intel && intel < 9)) {
      fprintf(stderr, "usage: %s [--sm SM] [--fill BYTE] [--nak 1] [--intel VER (9 or later)] "
                      "MODULE.air...\n", argv[0]);
      return 2;
   }
   glsl_type_singleton_init_or_ref();
   unsigned refused = 0;
#ifdef STELA_CORPUS_BRW
   if (intel)
      refused = sweep_intel(intel, argc - first, argv + first);
#endif
#ifdef STELA_CORPUS_NAK
   if (!intel) {
      const struct nv_device_info device = {.sm = (uint8_t)sm, .max_warps_per_mp = 48};
      struct nak_compiler *nak = nak_compiler_create(&device);
      if (!nak) {
         fprintf(stderr, "NAK refused SM %u\n", sm);
         return 2;
      }
      const struct stela_metal_target target = {
         .nir = STELA_METAL_NIR_ALL(nak_nir_options(nak)),
         .max_instructions = 1u << 20, .max_threadgroup_bytes = 49152,
         .max_private_bytes = 1u << 16, .max_constant_bytes = 65536, .max_varying_slots = 31,
         .max_payload_bytes = 16384, .max_mesh_vertices = 256, .max_mesh_primitives = 512,
         .raster_order_groups = 1, .fragment_tests = 1,
      };
      census = nak_census ? nak : NULL;
      for (int i = first; i < argc; i++)
         refused += sweep(argv[i], &target);
      nak_compiler_destroy(nak);
   }
#endif
   (void)sm;
   (void)nak_census;
   glsl_type_singleton_decref();
   printf("%u refused\n", refused);
   return refused != 0;
}
