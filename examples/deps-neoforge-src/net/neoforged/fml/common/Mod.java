package net.neoforged.fml.common;
import java.lang.annotation.*;
/** Compile-time stub of NeoForge's @Mod (not shipped in any fixture jar). */
@Retention(RetentionPolicy.RUNTIME) @Target(ElementType.TYPE)
public @interface Mod { String value(); net.neoforged.api.distmarker.Dist[] dist() default {}; }
