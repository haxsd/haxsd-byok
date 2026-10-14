/**
 * 模型库统计条里该出现哪些数字。
 *
 * 一个数字只有能回答问题时才占位置：没有内置模型就不写「模型 0」和「供应商 0」，
 * 没有插件模型就不写「插件模型 0」，只有真的测出了结果（成功或失败）才写通过/失败
 * ——取消掉的测试也算"有结果"，但「通过 0、失败 0」只是噪音。
 *
 * 判断独立成模块，方便直接对这张"哪些数字该出现"的规则断言，而不必渲染整页。
 */
export type ModelLibraryFact = "models" | "providers" | "pluginModels" | "testsPassed" | "testsFailed";

export function modelLibraryFacts(input: {
  modelCount: number;
  providerCount: number;
  pluginModelCount: number;
  testedOk: number;
  testedFailed: number;
}): ModelLibraryFact[] {
  return [
    ...(input.modelCount > 0 ? ["models" as const] : []),
    ...(input.providerCount > 0 ? ["providers" as const] : []),
    ...(input.pluginModelCount > 0 ? ["pluginModels" as const] : []),
    ...(input.testedOk > 0 || input.testedFailed > 0 ? ["testsPassed" as const, "testsFailed" as const] : []),
  ];
}
