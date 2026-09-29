package com.anjia.unidbgserver.utils;

import lombok.extern.slf4j.Slf4j;
import org.springframework.core.io.ClassPathResource;
import org.springframework.util.DigestUtils;
import org.springframework.util.StreamUtils;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.util.HashMap;
import java.util.Map;


/**
 * 资源解析工具：从 classpath 复制所需资源到临时文件并缓存复用。
 *
 * <p>番茄签名/下载运行期仅依赖 classpath 内的 libmetasec_ml.so / libc++_shared.so /
 * ms_16777218.bin（均已随 jar 打包）。经实验证实签名与章节下载均【不读取 base.apk】，
 * 故此处不再处理 APK，也不引入外部资产目录/运行时抽取逻辑。
 */
@Slf4j
public class TempFileUtils {

    private static final Map<String, File> TEMP_FILES = new HashMap<>();

    /**
     * 获取临时文件。如果缓存中不存在，则从 classpath 复制。
     *
     * @param classpathFile classpath 下的资源路径
     * @return 临时文件对象；资源缺失时返回 null
     */
    public static File getTempFile(String classpathFile) {
        try {
            String md5 = DigestUtils.md5DigestAsHex(classpathFile.getBytes());
            if (TEMP_FILES.containsKey(md5)) {
                return TEMP_FILES.get(md5);
            }

            ClassPathResource resource = new ClassPathResource(classpathFile);
            if (!resource.exists()) {
                log.error("资源文件不存在: {}", classpathFile);
                return null;
            }

            String extension = "";
            int dotIndex = classpathFile.lastIndexOf(".");
            if (dotIndex > 0) {
                extension = classpathFile.substring(dotIndex);
            }

            File tempFile = File.createTempFile("unidbg_", extension);
            tempFile.deleteOnExit();

            try (InputStream is = resource.getInputStream();
                 FileOutputStream fos = new FileOutputStream(tempFile)) {
                StreamUtils.copy(is, fos);
            }

            TEMP_FILES.put(md5, tempFile);
            log.debug("临时文件创建成功: {} -> {}", classpathFile, tempFile.getAbsolutePath());
            return tempFile;
        } catch (IOException e) {
            log.error("创建临时文件失败: " + classpathFile, e);
            return null;
        }
    }

    /**
     * 清理所有临时文件。
     */
    public static void cleanup() {
        for (File file : TEMP_FILES.values()) {
            try {
                if (file.exists() && !file.delete()) {
                    file.deleteOnExit();
                }
            } catch (Exception e) {
                log.warn("删除临时文件失败: {}", file.getAbsolutePath(), e);
            }
        }
        TEMP_FILES.clear();
    }
}
