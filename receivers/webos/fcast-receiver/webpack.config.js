const webpack = require('webpack');
const path = require('path');
const CopyWebpackPlugin = require("copy-webpack-plugin");

// Build issues:
// * Must use '--no-minify' when packaging since packaging would break otherwise...
const buildMode = 'production';
// const buildMode = 'development';

// const TARGET = 'electron';
const TARGET = 'webOS';
// const TARGET = 'tizenOS';

module.exports = [
    {
        mode: buildMode,
        entry: {
            preload: './src/main/Preload.ts',
            renderer: './src/main/Renderer.ts',
        },
        target: 'web',
        module: {
            rules: [
                {
                    test: /\.tsx?$/,
                    include: [path.resolve(__dirname, '../../common/web'), path.resolve(__dirname, 'src')],
                    use: [{ loader: 'ts-loader' }]
                }
            ],
        },
        resolve: {
            alias: {
                'src': path.resolve(__dirname, 'src'),
                'lib': path.resolve(__dirname, 'lib'),
                'modules': path.resolve(__dirname, 'node_modules'),
                'common': path.resolve(__dirname, '../../common/web'),
            },
            extensions: ['.tsx', '.ts', '.js'],
        },
        output: {
            filename: '[name].js',
            // NOTE: `dist/main` seems to be a reserved directory on the LGTV device??? Access denied errors otherwise when reading from main directory...
            path: path.resolve(__dirname, 'dist/main_window'),
        },
        plugins: [
            new CopyWebpackPlugin({
                patterns: [
                    // Common assets
                    {
                        from: '../common/assets/**',
                        to: '../[path][name][ext]',
                        context: path.resolve(__dirname, '..', '..', 'common'),
                        globOptions: { ignore: ['**/*.txt'] }
                    },
                    {
                        from: '../../common/web/main/common.css',
                        to: '[name][ext]',
                    },
                    // Target assets
                    { from: 'appinfo.json', to: '../[name][ext]' },
                    {
                        from: '**',
                        to: '../assets/[path][name][ext]',
                        context: path.resolve(__dirname, 'assets'),
                    },
                    {
                        from: '**',
                        to: '../lib/[name][ext]',
                        context: path.resolve(__dirname, 'lib'),
                        globOptions: { ignore: ['**/*.txt'] }
                    },
                    {
                        from: './src/main/*',
                        to: '[name][ext]',
                        globOptions: { ignore: ['**/*.ts'] }
                    }
                ],
            }),
            new webpack.DefinePlugin({
                TARGET: JSON.stringify(TARGET)
            })
        ]
    },
    {
        mode: buildMode,
        entry: {
            preload: './src/player/Preload.ts',
            renderer: './src/player/Renderer.ts',
        },
        target: 'web',
        module: {
            rules: [
                {
                    test: /\.tsx?$/,
                    include: [path.resolve(__dirname, '../../common/web'), path.resolve(__dirname, 'src')],
                    use: [{ loader: 'ts-loader' }]
                }
            ],
        },
        resolve: {
            alias: {
                'src': path.resolve(__dirname, 'src'),
                'lib': path.resolve(__dirname, 'lib'),
                'modules': path.resolve(__dirname, 'node_modules'),
                'common': path.resolve(__dirname, '../../common/web'),
            },
            extensions: ['.tsx', '.ts', '.js'],
        },
        output: {
            filename: '[name].js',
            path: path.resolve(__dirname, 'dist/player'),
        },
        plugins: [
            new CopyWebpackPlugin({
                patterns: [
                    {
                        from: '../../common/web/player/common.css',
                        to: '[name][ext]',
                    },
                    {
                        from: './src/player/*',
                        to: '[name][ext]',
                        globOptions: { ignore: ['**/*.ts'] }
                    }
                ],
            }),
            new webpack.DefinePlugin({
                TARGET: JSON.stringify(TARGET)
            })
        ]
    },
];
