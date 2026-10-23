import { defineProviderPlugin } from "cursor-byok:plugin";
import { claudeCodeOAuth } from "./oauth.ts";
import { claudeCodeProvider } from "./provider.ts";
import { presentAccount, refreshAccount, RESOURCE_TYPE } from "./resources.ts";

/**
 * 不提供“从 Claude Code 本地凭证文件导入”：该文件的字段没有公开稳定契约，
 * 只按记忆猜格式会重蹈假导入的覆辙。需要迁移时先在本插件走一次 OAuth 登录。
 */
export default defineProviderPlugin({
  providers: [claudeCodeProvider],
  resources: [{
    type: RESOURCE_TYPE,
    displayName: {
      "en-US": "Claude subscription accounts",
      "zh-CN": "Claude 订阅账号",
      "ru-RU": "Аккаунты подписки Claude",
    },
    add: [claudeCodeOAuth],
    present: presentAccount,
    refresh: refreshAccount,
  }],
});
