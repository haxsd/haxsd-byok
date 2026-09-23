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
import styles from "./TutorialPage.module.scss";

type Step = {
  key: string;
  title: string;
  detail: string;
  done: boolean;
  /** 已经做到哪一步，用一句话说清（未完成时给下一步动作）。 */
  status: string;
  path: string;
  action: string;
};

/**
 * 产品自己的使用教程。
 *
 * 以前这个入口打开的是**上游产品的文档站**：那里的步骤、截图和概念都不是这个应用的，
 * 用户照着做会对不上界面。这一页写的是这个产品的真实流程，而且会**读当前状态**——
 * 已经做完的步骤直接打勾，并指出下一步该去哪一页。
 */
export function TutorialPage() {
  const { models, cursorHarness, devinStatus, overview, calls } = useAppStore();
  const navigate = useNavigate();
  const [hostStatus, setHostStatus] = useState<DevinHostPatchStatus | null>(null);

  useEffect(() => {
    let cancelled = false;
    void api.devinHostStatus()
      .then((status) => { if (!cancelled) setHostStatus(status); })
      .catch(() => { if (!cancelled) setHostStatus(null); });
    return () => { cancelled = true; };
  }, []);

  const caReady = cursorHarness?.ca === "ready";
  const cursorTakenOver = cursorHarness?.settings_applied ?? false;
  const gatewayUp = Boolean(devinStatus?.enabled && devinStatus?.listening);
  const hostPatched = hostStatus?.patched ?? false;
  const hasCalls = calls.length > 0 || overview.metrics.llm_calls > 0;

  const steps: Step[] = [
    {
      key: "model",
      title: t("添加一个模型"),
      detail: t("模型库里的一项配置就是一个可用的模型：服务器地址、API Key、模型 ID。Cursor 与 Devin 共用这一份。"),
      done: models.length > 0,
      status: models.length > 0 ? t("已有 {count} 个模型", { count: models.length }) : t("还没有模型，任何客户端都无从回答"),
      path: "/models",
      action: t("去模型库"),
    },
    {
      key: "cursor",
      title: t("让 Cursor 用上它"),
      detail: t("两件事：生成并信任本地 CA（用于读取 Cursor 的 HTTPS 请求），然后打开接管开关把 Cursor 的请求引到本机。"),
      done: caReady && cursorTakenOver,
      status: cursorTakenOver
        ? t("已接管")
        : caReady
          ? t("证书已就绪，还差打开接管开关")
          : t("本地 CA 还没就绪"),
      path: "/harness/cursor",
      action: t("去 Cursor 页"),
    },
    {
      key: "devin",
      title: t("让 Devin 用上它"),
      detail: t("三件事：启用本机网关、把 Devin 的模型标识绑定到模型库、给 Devin 的宿主文件打补丁。绑定之后 Devin 才认识这些模型。"),
      done: gatewayUp && hostPatched,
      status: !devinStatus?.enabled
        ? t("网关未启用")
        : !devinStatus.listening
          ? t("网关已启用，等待端口就绪")
          : hostPatched
            ? t("已接通")
            : t("网关在运行，还差宿主补丁"),
      path: "/harness/devin",
      action: t("去 Devin 页"),
    },
    {
      key: "verify",
      title: t("在客户端里跑一次对话"),
      detail: t("回到 Cursor 或 Devin 里发一句话。成功后回到「调用」页，会看到这次调用的模型、耗时与 Token。"),
      done: hasCalls,
      status: hasCalls ? t("已有 {count} 条调用记录", { count: calls.length }) : t("还没有调用记录"),
      path: "/calls",
      action: t("去调用页"),
    },
  ];
  const completed = steps.filter((step) => step.done).length;

  const faq: Array<{ question: string; answer: string }> = [
    {
      question: t("模型测试失败了怎么办？"),
      answer: t("先看错误信息：401/403 多半是 Key 或套餐不匹配（有些厂商的编程套餐 Key 与普通 Key 不通用），404 是地址多了或少了路径，超时是网络或代理问题。模型卡上的「测试」会用一次真实请求验证，成功后会显示 tokens/s。"),
    },
    {
      question: t("Cursor 里看不到我的模型？"),
      answer: t("接管开关打开后需要手动重启 Cursor：配置在 Cursor 启动时读取。另外确认模型库里有模型，并且 Cursor 页的证书状态是「已就绪」。"),
    },
    {
      question: t("Devin 页显示端口未监听？"),
      answer: t("先保存设置：端口在保存后的下次启动才打开。如果已经重启过仍显示未监听，多半是端口被别的进程占用（本机其他路由器工具也会抢 43110-43112）。"),
    },
    {
      question: t("缓存命中率一直是 0？"),
      answer: t("缓存由模型提供方决定：只有在同一个对话里重复发送相同前缀时才会命中，短对话或每次都换上下文就不会有缓存读取。命中率是「缓存读取 /（缓存读取 + 非缓存输入）」。"),
    },
    {
      question: t("「价值估算」的钱是怎么算的？"),
      answer: t("用你在设置里填的单价乘实际用量。默认按高峰/低谷分时计价（UTC 工作日 01:00-04:00 与 06:00-10:00 为高峰），币种跟随界面语言：简体中文用人民币，英文用美元。它是估算，不是账单。"),
    },
    {
      question: t("退出软件后 Cursor / Devin 还能用吗？"),
      answer: t("开启接管期间，客户端被指向本机端口，软件不在运行时这些请求会失败。长期不用时建议先关闭接管开关（会把配置清理掉），需要时再打开。"),
    },
    {
      question: t("会影响机器上另一个同类软件吗？"),
      answer: t("端口、数据目录与安装目录都是独立的，它也只会清理自己写过的配置条目（凭自己留下的标记），卸载同样不动别人的配置。唯一的交汇点是 Cursor 的 settings.json：两个软件都往同一份文件里写代理设置。如果那份配置是对方写的，本应用不会覆盖它，只在 Cursor 页说明冲突；要由本应用接管，请先关闭对方的接管。"),
    },
    {
      question: t("数据存在哪？怎么清理？"),
      answer: t("都在本机：数据库在用户目录下的 .haxsd-byok-devin-v3 里。默认只记时间、状态与用量；开启「详细模式」才会保存完整请求与响应。清理入口在设置页的存储管理，可以只清详细记录或清空全部统计。"),
    },
  ];

  const glossary: Array<[string, string]> = [
    [t("自带密钥（BYOK）"), t("Bring Your Own Key：用你自己的模型服务，而不是厂商内置的模型。")],
    [t("接管"), t("把客户端的请求指向本机网关。对 Cursor 来说是改它的代理配置；对 Devin 来说是改宿主文件的端口。")],
    [t("本地 CA"), t("本机生成的一份证书，用于读取 Cursor 的 HTTPS 请求。它只保存在本机数据目录，也不会安装到系统证书库。")],
    [t("宿主补丁"), t("改写 Devin 自己的 extension.js，把它的上游地址指向本机端口。应用前会校验版本锚点并备份原文件。")],
    [t("缓存命中率"), t("缓存读取 /（缓存读取 + 非缓存输入）。它衡量的是「有多少输入是从提供方的缓存里读回来的」。")],
    [t("详细模式"), t("额外保存完整的请求头、请求体与响应流，便于排查问题；它会让数据库明显变大。")],
  ];

  const content = <div className={styles.page}>
    <Card className={styles.hero}>
      <div className={styles.heroText}>
        <span className={styles.heroEyebrow}>{t("这是什么")}</span>
        <h2>{t("把 Cursor 和 Devin 接到你自己的模型上")}</h2>
        <p>{t("两个客户端都只认厂商的服务器与模型。本应用在本机拦下模型请求，用你配置的模型回答；登录、账号、遥测这些请求原样转发给厂商，所以厂商登录照常可用。")}</p>
        <p>{t("Cursor 与 Devin 是两个并行的模块：各自独立的网关、端口与开关，互不影响；它们共用同一份模型库和同一份统计数据。")}</p>
      </div>
      <div className={styles.heroProgress}>
        <span className={styles.heroCount}>{completed}<small>/{steps.length}</small></span>
        <span className={styles.heroCountLabel}>{t("已完成的上手步骤")}</span>
        <Meter
          height={6}
          ariaLabel={t("上手进度 {done}/{total}", { done: completed, total: steps.length })}
          segments={[{ value: completed, color: "var(--oa-ok)", label: t("已完成") }, { value: steps.length - completed, color: "color-mix(in srgb, var(--vscode-foreground) 12%, transparent)", label: t("待完成") }]}
        />
      </div>
    </Card>

    <TitledCard
      title={t("上手清单")}
      description={t("按顺序做即可；这一步的状态会跟着你的实际操作变化。")}
      action={<Button size="small" onClick={() => void appStore.refresh()}>{t("刷新状态")}</Button>}
    >
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
          { key: "client", label: t("客户端"), detail: "Cursor / Devin", state: "up" },
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
          <span>{t("换模型不用改客户端：在模型库里改完，两个客户端下次请求就会用新的。")}</span>
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
      status={<StatusPill tone={completed === steps.length ? "ok" : "idle"}>{t("上手进度 {done}/{total}", { done: completed, total: steps.length })}</StatusPill>}
      meta={t("从零到第一条调用记录的完整流程，含常见问题与术语")}
    />}
    sections={[{ key: "tutorial", estimatedHeight: 1400, content }]}
  />;
}
