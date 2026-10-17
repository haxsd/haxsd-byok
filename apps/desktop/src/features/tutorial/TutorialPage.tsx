import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api, type DevinHostPatchStatus } from "../../shared/api";
import { PageContent } from "../../shell/layout/PageContent";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { Card } from "../../shared/ui/Card";
import { Icon } from "../../shared/ui/Icon";
import { Meter } from "../../shared/ui/ProgressRing";
import { PageTitle } from "../../shared/ui/PageTitle";
import { StatusHero } from "../../shared/ui/StatusHero";
import { StatusPill } from "../../shared/ui/StatusPill";
import { TitledCard } from "../../shared/ui/TitledCard";
import { arrowRightIcon, checkCircleIcon, refreshIcon } from "../../shared/ui/icons";
import { navCallsIcon, navDevinIcon, navModelsIcon, navSettingsIcon } from "../../shared/ui/navIcons";
import { buildOnboarding, type OnboardingClient } from "./onboarding";
import styles from "./TutorialPage.module.scss";

/**
 * 客户端选择的顺序与接入页一致。客户端名是固定产品名，不参与翻译；
 * 顺序写在这里，而不是从各页面推断，接入模块之间互不依赖。
 */
const onboardingClients: Array<{ id: OnboardingClient; label: string }> = [
  { id: "cursor", label: "Cursor" },
  { id: "devin", label: "Devin" },
  { id: "codex", label: "Codex" },
];

/**
 * 产品自己的使用教程。
 *
 * 以前这个入口打开的是**上游产品的文档站**：那里的步骤、截图和概念都不是这个应用的，
 * 用户照着做会对不上界面。这一页写的是这个产品的真实流程，而且会**读当前状态**——
 * 已经做完的步骤直接打勾，并指出下一步该去哪一页。
 *
 * Cursor、Devin 和 Codex 是三条独立的接入路径，所以先选接入模块：只有选定的那一条
 * 进入清单与进度，其余两条不会被当成欠下的待办。选择只存在于页面状态里。
 */
export function TutorialPage() {
  const { models, cursorHarness, devinStatus, codexStatus, overview, calls } = useAppStore();
  const navigate = useNavigate();
  const [hostStatus, setHostStatus] = useState<DevinHostPatchStatus | null>(null);
  // 读取过程单独记一份：读不到和「确实没打补丁」必须分开，否则清单会把读失败
  // 说成「还差宿主补丁」，让人去重打一次会改写 Devin 文件的补丁。
  const [hostStatusRead, setHostStatusRead] = useState<"reading" | "failed" | null>("reading");
  const [hostAttempt, setHostAttempt] = useState(0);
  const [client, setClient] = useState<OnboardingClient | null>(null);

  // 计数由「刷新状态」按钮推进，让宿主补丁状态跟着重读；卸载守卫保持原样。
  useEffect(() => {
    let cancelled = false;
    setHostStatusRead("reading");
    void api.devinHostStatus()
      .then((status) => {
        if (cancelled) return;
        setHostStatus(status);
        setHostStatusRead(null);
      })
      .catch(() => {
        if (cancelled) return;
        setHostStatus(null);
        setHostStatusRead("failed");
      });
    return () => { cancelled = true; };
  }, [hostAttempt]);

  const { steps, completed, total } = buildOnboarding({
    client,
    modelCount: models.length,
    cursor: cursorHarness ? {
      caReady: cursorHarness.ca === "ready",
      settingsApplied: cursorHarness.settings_applied,
      mode: cursorHarness.mode,
    } : null,
    devin: devinStatus ? { enabled: devinStatus.enabled, listening: devinStatus.listening } : null,
    codex: codexStatus ? { enabled: codexStatus.enabled, listening: codexStatus.listening, configured: codexStatus.configured } : null,
    hostPatched: hostStatus?.patched ?? false,
    hostPatchRead: hostStatusRead,
    callCount: Math.max(calls.length, overview.metrics.llm_calls),
  });

  const faq: Array<{ question: string; answer: string }> = [
    {
      question: t("模型测试失败了怎么办？"),
      answer: t("先看错误信息：401/403 多半是 Key 或套餐不匹配（有些厂商的编程套餐 Key 与普通 Key 不通用），404 多半是地址写错了（多写或少写了路径），超时多半是网络或代理问题。模型卡上的「测试」会用一次真实请求验证，成功后会显示 tokens/s。"),
    },
    {
      question: t("Cursor 里看不到我的模型？"),
      answer: t("接管开关打开后需要手动重启 Cursor：配置在 Cursor 启动时读取。另外确认模型库里有模型，并且 Cursor 页的证书状态是「已就绪」。"),
    },
    {
      question: t("Devin 页显示端口未监听？"),
      answer: t("先保存设置：端口在保存后的下次启动才打开。如果已经重启过仍显示未监听，多半是端口被别的进程占用（本机其他同类软件也会抢 43110-43112）。"),
    },
    {
      question: t("缓存命中率一直是 0？"),
      answer: t("缓存由模型提供方决定：只有在同一个对话里重复发送相同前缀时才会命中，短对话或每次都换上下文就不会有缓存读取。命中率是「缓存读取 /（缓存读取 + 非缓存输入）」。"),
    },
    {
      question: t("「价值估算」的钱是怎么算的？"),
      answer: t("用你在设置里为每个模型填的单价乘实际用量；没填单价的模型不计入金额。计价方式（全时段固定，或高峰与低谷分别计价）由你为每个模型选择，币种跟随界面语言。它是估算，不是账单。"),
    },
    {
      question: t("退出软件后 Cursor、Devin 和 Codex 还能用吗？"),
      answer: t("退出时会撤掉 Cursor 与 Codex 的接管配置，它们回到厂商直连；Devin 的宿主补丁会保留，在恢复原文件之前它的请求会失败。不再使用时，建议先在对应接入页撤销接管。"),
    },
    {
      question: t("会影响机器上另一个同类软件吗？"),
      answer: t("端口、数据目录与安装目录都是独立的，卸载也只清理自己写过的配置条目。改写的是客户端自己的配置文件（Cursor 的 settings.json、Codex 的 config.toml、Devin 的宿主文件）：如果是别的软件写入的，本应用不会覆盖，只在对应接入页说明冲突；要由本应用接管，请先关闭对方的接管。"),
    },
    {
      question: t("数据存在哪？怎么清理？"),
      answer: t("都在本机用户目录下的 .haxsd-byok-devin-v3 里。默认只记录时间、状态与用量，开启「详细模式」才会保存完整请求与响应；清理入口在设置页的存储管理。"),
    },
  ];

  const glossary: Array<[string, string]> = [
    [t("自带密钥（BYOK）"), t("Bring Your Own Key：用你自己的模型服务，而不是厂商内置的模型。")],
    [t("接管"), t("把客户端的请求指向本机网关。对 Cursor 来说是改它的代理配置，对 Codex 来说是改写 config.toml 里的模型路由，对 Devin 来说是改宿主文件的端口。")],
    [t("本地 CA"), t("本机生成的一份证书，用于读取 Cursor 的 HTTPS 请求。它需要安装到系统信任库（Windows 受信任的根证书颁发机构、macOS 钥匙串或 Linux 系统证书目录）才能让 Cursor 信任它；安装会请你确认。")],
    [t("宿主补丁"), t("改写 Devin 自己的 extension.js，把它的上游地址指向本机端口。应用前会校验版本锚点并备份原文件。")],
    [t("缓存命中率"), t("缓存读取 /（缓存读取 + 非缓存输入）。它衡量的是「有多少输入是从提供方的缓存里读回来的」。")],
    [t("详细模式"), t("额外保存完整的请求头、请求体与响应流，便于排查问题；它会让数据库明显变大。")],
  ];

  const content = <div className={styles.page}>
    <Card className={styles.hero}>
      <div className={styles.heroText}>
        <span className={styles.heroEyebrow}>{t("这是什么")}</span>
        <h2>{t("把 Cursor、Devin 和 Codex 接到你自己的模型上")}</h2>
        <p>{t("三个客户端都只认厂商的服务器与模型。本应用在本机拦下模型请求，用你配置的模型回答；登录、账号、遥测这些请求原样转发，厂商登录照常可用。三个客户端各自独立接入，共用同一份模型库与统计数据。")}</p>
      </div>
      <div className={styles.heroProgress}>
        {client === null ? <span className={styles.heroPrompt}>{t("先选择接入模块")}</span> : <>
          <span className={styles.heroCount}>{completed}<small>/{total}</small></span>
          <span className={styles.heroCountLabel}>{t("已完成的上手步骤")}</span>
          <Meter
            height={6}
            ariaLabel={t("上手进度 {done}/{total}", { done: completed, total })}
            segments={[{ value: completed, color: "var(--oa-ok)", label: t("已完成") }, { value: total - completed, color: "color-mix(in srgb, var(--vscode-foreground) 12%, transparent)", label: t("待完成") }]}
          />
        </>}
      </div>
    </Card>

    <TitledCard
      title={t("上手清单")}
      description={t("按顺序做即可；这一步的状态会跟着你的实际操作变化。")}
      action={<Button size="small" onClick={() => { void appStore.refresh(); setHostAttempt(value => value + 1); }}>{t("刷新状态")}</Button>}
    >
      <div className={styles.clientPicker}>
        <span className={styles.clientPickerLabel} id="tutorial-client-label">{t("先选择接入模块")}</span>
        <div className={styles.clientOptions} role="group" aria-labelledby="tutorial-client-label">
          {onboardingClients.map((option) => <button
            key={option.id}
            type="button"
            className={styles.clientOption}
            data-active={client === option.id || undefined}
            aria-pressed={client === option.id}
            onClick={() => setClient(option.id)}
          >{option.label}</button>)}
        </div>
        <span className={styles.clientHint}>{t("清单只跟随所选接入模块，其他接入模块不计入进度。")}</span>
      </div>
      <ol className={styles.steps}>
        {steps.map((step, index) => <li key={step.key} className={styles.step} data-done={step.done || undefined}>
          <span className={styles.stepMark} aria-hidden="true">
            {step.done ? <Icon icon={checkCircleIcon} size="1.3em" /> : <span className={styles.stepIndex}>{index + 1}</span>}
          </span>
          <div className={styles.stepBody}>
            <div className={styles.stepTitle}>
              <strong>{step.title}</strong>
              <StatusPill tone={step.done ? "ok" : "idle"}>{step.status}</StatusPill>
            </div>
            <p>{step.detail}</p>
          </div>
          <button type="button" className={styles.stepAction} onClick={() => void navigate(step.path)}>
            {step.action}<Icon icon={arrowRightIcon} size="1em" />
          </button>
        </li>)}
      </ol>
    </TitledCard>

    <TitledCard title={t("请求怎么走")} description={t("只有模型请求被本地处理，其余原样转发。")}>
      <StatusHero
        connected
        title={t("一次请求的三段路")}
        description={t("客户端把请求发给本机网关，网关按你的配置调用模型服务，再把结果原样流回客户端。")}
        stages={[
          { key: "client", label: t("客户端"), detail: "Cursor / Devin / Codex", state: "up" },
          { key: "gateway", label: t("本机网关"), detail: t("改写模型与鉴权，其余透传"), state: "up" },
          { key: "model", label: t("你的模型"), detail: t("模型库里配置的服务"), state: "up" },
        ]}
        steps={[]}
        footnotes={t("网关固定监听 127.0.0.1，不对外网开放。")}
      />
    </TitledCard>

    <TitledCard title={t("常见问题")} description={t("按出现频率排序，点开看答案。")}>
      <div className={styles.faq}>
        {faq.map((item) => <details key={item.question} className={styles.faqItem}>
          <summary>{item.question}</summary>
          <p>{item.answer}</p>
        </details>)}
      </div>
    </TitledCard>

    <TitledCard title={t("术语表")} description={t("界面里反复出现、但不一定都眼熟的六个词。")}>
      <dl className={styles.glossary}>
        {glossary.map(([term, meaning]) => <div key={term}>
          <dt>{term}</dt>
          <dd>{meaning}</dd>
        </div>)}
      </dl>
    </TitledCard>

    <TitledCard title={t("还没解决的问题？")} description={t("自己排查比猜更省时间。")}>
      <ul className={styles.helpList}>
        <li>
          <Icon icon={navCallsIcon} size="1.1em" />
          <span>{t("调用页能看到每次调用的完整字段：错误信息、HTTP 状态、TTFB 与 Token 构成。先在设置里打开「详细模式」，还能看到请求与响应原文。")}</span>
        </li>
        <li>
          <Icon icon={navSettingsIcon} size="1.1em" />
          <span>{t("设置页可以切换主题与语言、改端口、配置出网代理、清理统计数据。")}</span>
        </li>
        <li>
          <Icon icon={navModelsIcon} size="1.1em" />
          <span>{t("换模型不用改客户端：在模型库里改完，接入模块下次请求就会用新的。")}</span>
        </li>
        <li>
          <Icon icon={navDevinIcon} size="1.1em" />
          <span>{t("Devin 的宿主补丁可以随时「恢复原文件」，恢复后 Devin 回到厂商直连。")}</span>
        </li>
        <li>
          <Icon icon={refreshIcon} size="1.1em" />
          <span>{t("「调用」页每 2 秒自动刷新一次，其他页面的数字在打开页面或点右上角刷新时更新；看到「服务未连接」说明本地管理服务没起来，先重启应用。")}</span>
        </li>
      </ul>
    </TitledCard>
  </div>;

  return <PageContent
    title={<PageTitle
      title={t("使用教程")}
      status={client === null ? undefined : <StatusPill tone={completed === total ? "ok" : "idle"}>{t("上手进度 {done}/{total}", { done: completed, total })}</StatusPill>}
      meta={t("从零到第一条调用记录的完整流程，含常见问题与术语")}
    />}
    sections={[{ key: "tutorial", estimatedHeight: 1400, content }]}
  />;
}
