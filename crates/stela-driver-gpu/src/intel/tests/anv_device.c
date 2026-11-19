/* SPDX-License-Identifier: GPL-2.0-or-later */
/* ANV on xe or i915 with no GPU: the image's route to a device, run on the host.
 * The program links ANV's objects as Mesa links its driver, Stela's libdrm
 * stand-in (compat/mesa/drm.c) in place of libdrm, and Stela's NIR options
 * hook (compat/mesa/anv_nir_options.c), and runs over the kernel driver's
 * no-op shim (intel/hosted/xe_noop_drm_shim.c or i915_noop_drm_shim.c)
 * preloaded. It creates the instance and
 * the device of the one GPU the shim shows, checks what Vulkan reports of it
 * against the expected device, asks the hook for each stage's NIR options,
 * builds a compute pipeline from SPIR-V (BRW compiles it; its statistics count
 * the instructions and code bytes) and one from NIR built with the hook's
 * options, as the Metal device passes the AIR compiler's NIR
 * (intel/tests/anv_nir_shader.c: a formatless storage image read, which
 * Metal's read-write textures need), and records, submits and waits for one
 * dispatch, which the shim accepts without running it.
 *
 * It also says which of the Metal device's Vulkan requirements ANV does not
 * meet on the device, and which of Metal's tier 2 read-write texture formats
 * ANV cannot read and write without a format, which is no failure of this
 * test: the Metal device refuses such a device, or reports the tier absent,
 * when it opens.
 *
 * Usage: anv_device DEVICE-ID integrated|discrete|none xe|i915
 * Prints a "Metal device: ..." line, a "Metal read-write textures: ..." line
 * and a line "ANV on <driver>: ..." and exits 0, or names the failed check.
 * With "none" (a device Stela hands to no driver) it checks that ANV gives no
 * Vulkan device and prints "no Vulkan device from ANV". */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <vulkan/vulkan.h>

#include <stela/host.h>

#include "anv_device_shader.h"

#define CHECK(condition)                                                                      \
	do {                                                                                  \
		if (!(condition)) {                                                           \
			fprintf(stderr, "anv_device: %s:%d: %s\n", __FILE__, __LINE__, #condition); \
			exit(1);                                                              \
		}                                                                             \
	} while (0)
#define VK(call) CHECK((call) == VK_SUCCESS)

/* Mesa's shader stages up to compute (`mesa_shader_stage`), and its count of
 * every stage (`MESA_ALL_SHADER_STAGES`), past which the hook answers none. */
#define STAGES 6
#define ALL_STAGES 15

VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL vk_icdGetInstanceProcAddr(VkInstance, const char *);
const void *stela_anv_nir_options(VkPhysicalDevice physical, unsigned stage);
void *stela_anv_test_nir(const void *options);
void stela_anv_test_nir_free(void *shader);

/* Mesa's `VkPipelineShaderStageNirCreateInfoMESA` (vk_internal_exts.h): a
 * stage without a module takes its NIR from it. */
#define STRUCTURE_TYPE_NIR_STAGE ((VkStructureType)1000290001)
struct nir_stage {
	VkStructureType sType;
	const void *pNext;
	const void *nir;
};

static uint16_t expected_device;
/* How often the libdrm stand-in asked for the device: never would mean
 * another libdrm listed it. */
static unsigned listed;

/* What the image's Linux side answers through `stela_libc_drm_device`: the
 * one render node the shim shows, at the shim's PCI address. */
unsigned int stela_libc_drm_devices(void)
{
	return 1;
}

void stela_libc_drm_device(unsigned int index, struct stela_libc_drm_device *device)
{
	CHECK(index == 0);
	listed++;
	memset(device, 0, sizeof(*device));
	device->vendor = 0x8086;
	device->device = expected_device;
	device->slot = 2;
}

#define LOAD(instance, name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(instance, #name)

/* Whether the device lists the extension `name`. */
static int has_extension(VkInstance instance, VkPhysicalDevice physical, const char *name)
{
	LOAD(instance, vkEnumerateDeviceExtensionProperties);
	uint32_t count = 0;
	VK(vkEnumerateDeviceExtensionProperties(physical, NULL, &count, NULL));
	VkExtensionProperties *extensions = calloc(count, sizeof(*extensions));
	CHECK(extensions);
	VK(vkEnumerateDeviceExtensionProperties(physical, NULL, &count, extensions));
	int found = 0;
	for (uint32_t index = 0; index < count && !found; index++)
		found = !strcmp(extensions[index].extensionName, name);
	free(extensions);
	return found;
}

/* The Metal device's Vulkan requirements (`metal::vulkan::backend::requirements`
 * and the dma-buf import `metal::native` asks for) the device does not meet,
 * printed as one line: "Metal device: refused, ANV lacks A, B" or "Metal device:
 * every requirement met". The Metal device refuses a device that lacks any. */
static void report_metal_requirements(VkInstance instance, VkPhysicalDevice physical)
{
	LOAD(instance, vkGetPhysicalDeviceFeatures2);
	VkPhysicalDeviceDynamicRenderingUnusedAttachmentsFeaturesEXT unused = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DYNAMIC_RENDERING_UNUSED_ATTACHMENTS_FEATURES_EXT,
	};
	VkPhysicalDeviceConditionalRenderingFeaturesEXT conditional = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_CONDITIONAL_RENDERING_FEATURES_EXT,
		.pNext = &unused,
	};
	VkPhysicalDeviceRobustness2FeaturesEXT robustness = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ROBUSTNESS_2_FEATURES_EXT,
		.pNext = &conditional,
	};
	VkPhysicalDeviceMaintenance5FeaturesKHR maintenance5 = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MAINTENANCE_5_FEATURES_KHR,
		.pNext = &robustness,
	};
	VkPhysicalDeviceExtendedDynamicState2FeaturesEXT dynamic2 = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTENDED_DYNAMIC_STATE_2_FEATURES_EXT,
		.pNext = &maintenance5,
	};
	VkPhysicalDeviceExtendedDynamicStateFeaturesEXT dynamic = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTENDED_DYNAMIC_STATE_FEATURES_EXT,
		.pNext = &dynamic2,
	};
	VkPhysicalDeviceVulkan13Features v13 = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
		.pNext = &dynamic,
	};
	VkPhysicalDeviceVulkan12Features v12 = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
		.pNext = &v13,
	};
	VkPhysicalDeviceFeatures2 features = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
		.pNext = &v12,
	};
	vkGetPhysicalDeviceFeatures2(physical, &features);
	const VkPhysicalDeviceFeatures *core = &features.features;
	const struct {
		const char *name;
		int present;
	} requirements[] = {
		{"bufferDeviceAddress", v12.bufferDeviceAddress},
		{"timelineSemaphore", v12.timelineSemaphore},
		{"synchronization2", v13.synchronization2},
		{"shaderFloat16", v12.shaderFloat16},
		{"dynamicRendering", v13.dynamicRendering},
		{"descriptorIndexing", v12.descriptorIndexing && v12.runtimeDescriptorArray},
		{"descriptorBindingPartiallyBound", v12.descriptorBindingPartiallyBound},
		{"descriptorBindingUpdateAfterBind",
		 v12.descriptorBindingSampledImageUpdateAfterBind &&
			 v12.descriptorBindingStorageImageUpdateAfterBind &&
			 v12.descriptorBindingUniformTexelBufferUpdateAfterBind &&
			 v12.descriptorBindingStorageTexelBufferUpdateAfterBind &&
			 v12.descriptorBindingUpdateUnusedWhilePending},
		{"shaderImageArrayNonUniformIndexing",
		 v12.shaderSampledImageArrayNonUniformIndexing &&
			 v12.shaderStorageImageArrayNonUniformIndexing},
		{"extendedDynamicState",
		 dynamic.extendedDynamicState && dynamic2.extendedDynamicState2},
		{"maintenance5", v13.maintenance4 && maintenance5.maintenance5},
		{"vulkanMemoryModel", v12.vulkanMemoryModel},
		{"storageImageWithoutFormat",
		 core->shaderStorageImageReadWithoutFormat && core->shaderStorageImageWriteWithoutFormat},
		{"independentBlend", core->independentBlend},
		{"nullDescriptor", robustness.nullDescriptor},
		{"conditionalRendering", conditional.conditionalRendering},
		{"dynamicRenderingUnusedAttachments", unused.dynamicRenderingUnusedAttachments},
		{"hostQueryReset", v12.hostQueryReset},
		{"VK_EXT_external_memory_dma_buf",
		 has_extension(instance, physical, "VK_KHR_external_memory_fd") &&
			 has_extension(instance, physical, "VK_EXT_external_memory_dma_buf")},
	};
	int lacking = 0;
	for (unsigned index = 0; index < sizeof(requirements) / sizeof(requirements[0]); index++) {
		if (requirements[index].present)
			continue;
		printf("%s%s", lacking ? ", " : "Metal device: refused, ANV lacks ",
		       requirements[index].name);
		lacking = 1;
	}
	printf(lacking ? "\n" : "Metal device: every requirement met\n");
}

/* Metal's tier 2 read-write texture formats, tier 1's first
 * (`metal::capabilities::read_write_tier`), that the device cannot read and
 * write as storage images without a format, printed as one line. */
static void report_read_write_formats(VkInstance instance, VkPhysicalDevice physical)
{
	LOAD(instance, vkGetPhysicalDeviceFormatProperties2);
	const struct {
		const char *name;
		VkFormat format;
	} formats[] = {
		{"R32Float", VK_FORMAT_R32_SFLOAT},
		{"R32Uint", VK_FORMAT_R32_UINT},
		{"R32Sint", VK_FORMAT_R32_SINT},
		{"RGBA32Float", VK_FORMAT_R32G32B32A32_SFLOAT},
		{"RGBA32Uint", VK_FORMAT_R32G32B32A32_UINT},
		{"RGBA32Sint", VK_FORMAT_R32G32B32A32_SINT},
		{"RGBA16Float", VK_FORMAT_R16G16B16A16_SFLOAT},
		{"RGBA16Uint", VK_FORMAT_R16G16B16A16_UINT},
		{"RGBA16Sint", VK_FORMAT_R16G16B16A16_SINT},
		{"RGBA8Unorm", VK_FORMAT_R8G8B8A8_UNORM},
		{"RGBA8Uint", VK_FORMAT_R8G8B8A8_UINT},
		{"RGBA8Sint", VK_FORMAT_R8G8B8A8_SINT},
		{"R16Float", VK_FORMAT_R16_SFLOAT},
		{"R16Uint", VK_FORMAT_R16_UINT},
		{"R16Sint", VK_FORMAT_R16_SINT},
		{"R8Unorm", VK_FORMAT_R8_UNORM},
		{"R8Uint", VK_FORMAT_R8_UINT},
		{"R8Sint", VK_FORMAT_R8_SINT},
	};
	const VkFormatFeatureFlags2 needed = VK_FORMAT_FEATURE_2_STORAGE_IMAGE_BIT |
					     VK_FORMAT_FEATURE_2_STORAGE_READ_WITHOUT_FORMAT_BIT |
					     VK_FORMAT_FEATURE_2_STORAGE_WRITE_WITHOUT_FORMAT_BIT;
	int lacking = 0;
	for (unsigned index = 0; index < sizeof(formats) / sizeof(formats[0]); index++) {
		VkFormatProperties3 features = {
			.sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_3,
		};
		VkFormatProperties2 properties = {
			.sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2,
			.pNext = &features,
		};
		vkGetPhysicalDeviceFormatProperties2(physical, formats[index].format, &properties);
		if ((features.optimalTilingFeatures & needed) == needed)
			continue;
		printf("%s%s", lacking ? ", " : "Metal read-write textures: ANV cannot read and write ",
		       formats[index].name);
		lacking = 1;
	}
	printf(lacking ? " without a format\n"
		       : "Metal read-write textures: every tier 2 format read and written\n");
}

/* BRW's code for `pipeline`: its executables' instructions and code bytes. */
static void brw_statistics(VkDevice device,
			   PFN_vkGetPipelineExecutablePropertiesKHR vkGetPipelineExecutablePropertiesKHR,
			   PFN_vkGetPipelineExecutableStatisticsKHR vkGetPipelineExecutableStatisticsKHR,
			   VkPipeline pipeline, uint64_t *instructions, uint64_t *code_bytes)
{
	const VkPipelineInfoKHR pipeline_info = {
		.sType = VK_STRUCTURE_TYPE_PIPELINE_INFO_KHR,
		.pipeline = pipeline,
	};
	uint32_t executable_count = 0;
	VK(vkGetPipelineExecutablePropertiesKHR(device, &pipeline_info, &executable_count, NULL));
	CHECK(executable_count >= 1);
	*instructions = 0;
	*code_bytes = 0;
	for (uint32_t executable = 0; executable < executable_count; executable++) {
		const VkPipelineExecutableInfoKHR info = {
			.sType = VK_STRUCTURE_TYPE_PIPELINE_EXECUTABLE_INFO_KHR,
			.pipeline = pipeline,
			.executableIndex = executable,
		};
		VkPipelineExecutableStatisticKHR statistics[32];
		uint32_t statistic_count = 32;
		for (uint32_t i = 0; i < statistic_count; i++)
			statistics[i] = (VkPipelineExecutableStatisticKHR){
				.sType = VK_STRUCTURE_TYPE_PIPELINE_EXECUTABLE_STATISTIC_KHR,
			};
		VkResult result =
			vkGetPipelineExecutableStatisticsKHR(device, &info, &statistic_count, statistics);
		CHECK(result == VK_SUCCESS || result == VK_INCOMPLETE);
		for (uint32_t i = 0; i < statistic_count; i++) {
			if (statistics[i].format != VK_PIPELINE_EXECUTABLE_STATISTIC_FORMAT_UINT64_KHR)
				continue;
			if (!strcmp(statistics[i].name, "Instructions"))
				*instructions += statistics[i].value.u64;
			else if (!strcmp(statistics[i].name, "Code size"))
				*code_bytes += statistics[i].value.u64;
		}
	}
	CHECK(*instructions > 0 && *code_bytes > 0);
}

int main(int argc, char **argv)
{
	CHECK(argc == 4);
	expected_device = (uint16_t)strtoul(argv[1], NULL, 16);
	VkPhysicalDeviceType expected_type = !strcmp(argv[2], "discrete")
						     ? VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
						     : VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU;

	LOAD(NULL, vkCreateInstance);
	const VkApplicationInfo application = {
		.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
		.apiVersion = VK_API_VERSION_1_3,
	};
	VkInstance instance;
	VK(vkCreateInstance(
		&(VkInstanceCreateInfo){
			.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
			.pApplicationInfo = &application,
		},
		NULL, &instance));
	LOAD(instance, vkEnumeratePhysicalDevices);
	LOAD(instance, vkGetPhysicalDeviceProperties2);
	LOAD(instance, vkGetPhysicalDeviceQueueFamilyProperties);
	LOAD(instance, vkGetPhysicalDeviceMemoryProperties);
	LOAD(instance, vkCreateDevice);
	LOAD(instance, vkGetDeviceProcAddr);
	LOAD(instance, vkDestroyInstance);

	uint32_t count = 0;
	VK(vkEnumeratePhysicalDevices(instance, &count, NULL));
	if (!strcmp(argv[2], "none")) {
		/* A device Stela hands to no driver: ANV gives it no Vulkan
		 * device. */
		CHECK(count == 0);
		puts("no Vulkan device from ANV");
		return 0;
	}
	CHECK(count == 1);
	VkPhysicalDevice physical;
	VK(vkEnumeratePhysicalDevices(instance, &count, &physical));
	CHECK(listed > 0);

	VkPhysicalDeviceDriverProperties driver = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES,
	};
	VkPhysicalDeviceProperties2 properties = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
		.pNext = &driver,
	};
	vkGetPhysicalDeviceProperties2(physical, &properties);
	CHECK(properties.properties.vendorID == 0x8086);
	CHECK(properties.properties.deviceID == expected_device);
	CHECK(properties.properties.deviceType == expected_type);
	CHECK(driver.driverID == VK_DRIVER_ID_INTEL_OPEN_SOURCE_MESA);
	report_metal_requirements(instance, physical);
	report_read_write_formats(instance, physical);

	/* The hook answers one set per stage, and none past Mesa's stages. */
	const void *options[STAGES];
	for (unsigned stage = 0; stage < STAGES; stage++) {
		options[stage] = stela_anv_nir_options(physical, stage);
		CHECK(options[stage]);
		for (unsigned other = 0; other < stage; other++)
			CHECK(options[other] != options[stage]);
	}
	CHECK(!stela_anv_nir_options(physical, ALL_STAGES));

	uint32_t families = 0;
	vkGetPhysicalDeviceQueueFamilyProperties(physical, &families, NULL);
	VkQueueFamilyProperties family_properties[8];
	CHECK(families && families <= 8);
	vkGetPhysicalDeviceQueueFamilyProperties(physical, &families, family_properties);
	uint32_t family = 0;
	while (family < families && !(family_properties[family].queueFlags & VK_QUEUE_COMPUTE_BIT))
		family++;
	CHECK(family < families);

	VkPhysicalDevicePipelineExecutablePropertiesFeaturesKHR executables = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PIPELINE_EXECUTABLE_PROPERTIES_FEATURES_KHR,
		.pipelineExecutableInfo = VK_TRUE,
	};
	const char *extensions[] = {VK_KHR_PIPELINE_EXECUTABLE_PROPERTIES_EXTENSION_NAME};
	VkDevice device;
	VK(vkCreateDevice(physical,
			  &(VkDeviceCreateInfo){
				  .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
				  .pNext = &executables,
				  .queueCreateInfoCount = 1,
				  .pQueueCreateInfos =
					  &(VkDeviceQueueCreateInfo){
						  .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
						  .queueFamilyIndex = family,
						  .queueCount = 1,
						  .pQueuePriorities = (float[]){1.0f},
					  },
				  .enabledExtensionCount = 1,
				  .ppEnabledExtensionNames = extensions,
			  },
			  NULL, &device));
#define DEVICE(name) PFN_##name name = (PFN_##name)vkGetDeviceProcAddr(device, #name)
	DEVICE(vkGetDeviceQueue);
	DEVICE(vkCreateShaderModule);
	DEVICE(vkCreateDescriptorSetLayout);
	DEVICE(vkCreatePipelineLayout);
	DEVICE(vkCreateComputePipelines);
	DEVICE(vkGetPipelineExecutablePropertiesKHR);
	DEVICE(vkGetPipelineExecutableStatisticsKHR);
	DEVICE(vkCreateBuffer);
	DEVICE(vkGetBufferMemoryRequirements);
	DEVICE(vkAllocateMemory);
	DEVICE(vkBindBufferMemory);
	DEVICE(vkMapMemory);
	DEVICE(vkCreateDescriptorPool);
	DEVICE(vkAllocateDescriptorSets);
	DEVICE(vkUpdateDescriptorSets);
	DEVICE(vkCreateCommandPool);
	DEVICE(vkAllocateCommandBuffers);
	DEVICE(vkBeginCommandBuffer);
	DEVICE(vkCmdBindPipeline);
	DEVICE(vkCmdBindDescriptorSets);
	DEVICE(vkCmdDispatch);
	DEVICE(vkEndCommandBuffer);
	DEVICE(vkCreateFence);
	DEVICE(vkQueueSubmit);
	DEVICE(vkWaitForFences);
	DEVICE(vkDeviceWaitIdle);
	DEVICE(vkDestroyFence);
	DEVICE(vkDestroyCommandPool);
	DEVICE(vkDestroyDescriptorPool);
	DEVICE(vkFreeMemory);
	DEVICE(vkDestroyBuffer);
	DEVICE(vkDestroyPipeline);
	DEVICE(vkDestroyPipelineLayout);
	DEVICE(vkDestroyDescriptorSetLayout);
	DEVICE(vkDestroyShaderModule);
	DEVICE(vkDestroyDevice);
	VkQueue queue;
	vkGetDeviceQueue(device, family, 0, &queue);

	VkShaderModule module;
	VK(vkCreateShaderModule(device,
				&(VkShaderModuleCreateInfo){
					.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
					.codeSize = sizeof(stela_anv_device_spirv),
					.pCode = stela_anv_device_spirv,
				},
				NULL, &module));
	VkDescriptorSetLayout set_layout;
	VK(vkCreateDescriptorSetLayout(
		device,
		&(VkDescriptorSetLayoutCreateInfo){
			.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
			.bindingCount = 1,
			.pBindings =
				&(VkDescriptorSetLayoutBinding){
					.binding = 0,
					.descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
					.descriptorCount = 1,
					.stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
				},
		},
		NULL, &set_layout));
	VkPipelineLayout layout;
	VK(vkCreatePipelineLayout(device,
				  &(VkPipelineLayoutCreateInfo){
					  .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
					  .setLayoutCount = 1,
					  .pSetLayouts = &set_layout,
				  },
				  NULL, &layout));
	VkPipeline pipeline;
	VK(vkCreateComputePipelines(
		device, VK_NULL_HANDLE, 1,
		&(VkComputePipelineCreateInfo){
			.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
			.flags = VK_PIPELINE_CREATE_CAPTURE_STATISTICS_BIT_KHR,
			.stage =
				{
					.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
					.stage = VK_SHADER_STAGE_COMPUTE_BIT,
					.module = module,
					.pName = "main",
				},
			.layout = layout,
		},
		NULL, &pipeline));

	/* BRW's code: each executable's instruction count. */
	uint64_t instructions, code_bytes;
	brw_statistics(device, vkGetPipelineExecutablePropertiesKHR,
		       vkGetPipelineExecutableStatisticsKHR, pipeline, &instructions, &code_bytes);

	/* The same through NIR: a formatless read of one storage image written to
	 * another, from NIR built with the hook's compute options. */
	VkDescriptorSetLayout image_layout;
	VK(vkCreateDescriptorSetLayout(
		device,
		&(VkDescriptorSetLayoutCreateInfo){
			.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
			.bindingCount = 2,
			.pBindings =
				(VkDescriptorSetLayoutBinding[]){
					{
						.binding = 0,
						.descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE,
						.descriptorCount = 1,
						.stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
					},
					{
						.binding = 1,
						.descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE,
						.descriptorCount = 1,
						.stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
					},
				},
		},
		NULL, &image_layout));
	VkPipelineLayout image_pipeline_layout;
	VK(vkCreatePipelineLayout(device,
				  &(VkPipelineLayoutCreateInfo){
					  .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
					  .setLayoutCount = 1,
					  .pSetLayouts = &image_layout,
				  },
				  NULL, &image_pipeline_layout));
	void *nir = stela_anv_test_nir(options[STAGES - 1]);
	CHECK(nir);
	const struct nir_stage nir_stage = {
		.sType = STRUCTURE_TYPE_NIR_STAGE,
		.nir = nir,
	};
	VkPipeline nir_pipeline;
	VK(vkCreateComputePipelines(
		device, VK_NULL_HANDLE, 1,
		&(VkComputePipelineCreateInfo){
			.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
			.flags = VK_PIPELINE_CREATE_CAPTURE_STATISTICS_BIT_KHR,
			.stage =
				{
					.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
					.pNext = &nir_stage,
					.stage = VK_SHADER_STAGE_COMPUTE_BIT,
					.pName = "main",
				},
			.layout = image_pipeline_layout,
		},
		NULL, &nir_pipeline));
	stela_anv_test_nir_free(nir);
	uint64_t nir_instructions, nir_code_bytes;
	brw_statistics(device, vkGetPipelineExecutablePropertiesKHR,
		       vkGetPipelineExecutableStatisticsKHR, nir_pipeline, &nir_instructions,
		       &nir_code_bytes);

	/* One dispatch over a host-visible buffer, submitted and waited for. */
	const VkDeviceSize bytes = 64 * sizeof(uint32_t);
	VkBuffer buffer;
	VK(vkCreateBuffer(device,
			  &(VkBufferCreateInfo){
				  .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
				  .size = bytes,
				  .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
			  },
			  NULL, &buffer));
	VkMemoryRequirements requirements;
	vkGetBufferMemoryRequirements(device, buffer, &requirements);
	VkPhysicalDeviceMemoryProperties memory;
	vkGetPhysicalDeviceMemoryProperties(physical, &memory);
	uint32_t type = 0;
	const VkMemoryPropertyFlags visible =
		VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
	while (type < memory.memoryTypeCount &&
	       (!(requirements.memoryTypeBits & (1u << type)) ||
		(memory.memoryTypes[type].propertyFlags & visible) != visible))
		type++;
	CHECK(type < memory.memoryTypeCount);
	VkDeviceMemory allocation;
	VK(vkAllocateMemory(device,
			    &(VkMemoryAllocateInfo){
				    .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
				    .allocationSize = requirements.size,
				    .memoryTypeIndex = type,
			    },
			    NULL, &allocation));
	VK(vkBindBufferMemory(device, buffer, allocation, 0));
	uint32_t *values;
	VK(vkMapMemory(device, allocation, 0, bytes, 0, (void **)&values));
	for (uint32_t i = 0; i < 64; i++)
		values[i] = i;

	VkDescriptorPool pool;
	VK(vkCreateDescriptorPool(device,
				  &(VkDescriptorPoolCreateInfo){
					  .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
					  .maxSets = 1,
					  .poolSizeCount = 1,
					  .pPoolSizes =
						  &(VkDescriptorPoolSize){
							  .type = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
							  .descriptorCount = 1,
						  },
				  },
				  NULL, &pool));
	VkDescriptorSet set;
	VK(vkAllocateDescriptorSets(device,
				    &(VkDescriptorSetAllocateInfo){
					    .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
					    .descriptorPool = pool,
					    .descriptorSetCount = 1,
					    .pSetLayouts = &set_layout,
				    },
				    &set));
	vkUpdateDescriptorSets(device, 1,
			       &(VkWriteDescriptorSet){
				       .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
				       .dstSet = set,
				       .descriptorCount = 1,
				       .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
				       .pBufferInfo =
					       &(VkDescriptorBufferInfo){
						       .buffer = buffer,
						       .range = bytes,
					       },
			       },
			       0, NULL);
	VkCommandPool commands;
	VK(vkCreateCommandPool(device,
			       &(VkCommandPoolCreateInfo){
				       .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
				       .queueFamilyIndex = family,
			       },
			       NULL, &commands));
	VkCommandBuffer command_buffer;
	VK(vkAllocateCommandBuffers(device,
				    &(VkCommandBufferAllocateInfo){
					    .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
					    .commandPool = commands,
					    .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
					    .commandBufferCount = 1,
				    },
				    &command_buffer));
	VK(vkBeginCommandBuffer(command_buffer,
				&(VkCommandBufferBeginInfo){
					.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
					.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
				}));
	vkCmdBindPipeline(command_buffer, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline);
	vkCmdBindDescriptorSets(command_buffer, VK_PIPELINE_BIND_POINT_COMPUTE, layout, 0, 1, &set,
				0, NULL);
	vkCmdDispatch(command_buffer, 1, 1, 1);
	VK(vkEndCommandBuffer(command_buffer));
	VkFence fence;
	VK(vkCreateFence(device,
			 &(VkFenceCreateInfo){
				 .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
			 },
			 NULL, &fence));
	VK(vkQueueSubmit(queue, 1,
			 &(VkSubmitInfo){
				 .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
				 .commandBufferCount = 1,
				 .pCommandBuffers = &command_buffer,
			 },
			 fence));
	VK(vkWaitForFences(device, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000));
	VK(vkDeviceWaitIdle(device));

	vkDestroyFence(device, fence, NULL);
	vkDestroyCommandPool(device, commands, NULL);
	vkDestroyDescriptorPool(device, pool, NULL);
	vkDestroyBuffer(device, buffer, NULL);
	vkFreeMemory(device, allocation, NULL);
	vkDestroyPipeline(device, nir_pipeline, NULL);
	vkDestroyPipelineLayout(device, image_pipeline_layout, NULL);
	vkDestroyDescriptorSetLayout(device, image_layout, NULL);
	vkDestroyPipeline(device, pipeline, NULL);
	vkDestroyPipelineLayout(device, layout, NULL);
	vkDestroyDescriptorSetLayout(device, set_layout, NULL);
	vkDestroyShaderModule(device, module, NULL);
	vkDestroyDevice(device, NULL);
	vkDestroyInstance(instance, NULL);
	printf("ANV on %s: %s (0x%04x, %s), NIR options for %u stages, compute pipeline of %llu "
	       "instructions in %llu bytes from SPIR-V and of %llu instructions in %llu bytes "
	       "from NIR, one dispatch submitted\n",
	       argv[3], properties.properties.deviceName, expected_device, argv[2], STAGES,
	       (unsigned long long)instructions, (unsigned long long)code_bytes,
	       (unsigned long long)nir_instructions, (unsigned long long)nir_code_bytes);
	return 0;
}
